use crate::hardware::backends::hwmon::resolve_hwmon;
use crate::{
    config::{FanConfig, FanSelector},
    policy::control::{FanCommand, Speed},
};
use anyhow::{Context, Result, ensure};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

pub trait FanDevice {
    fn apply(&mut self, speed: &Speed) -> Result<()>;
    fn restore(&mut self) -> Result<()>;
    fn emergency(&mut self) -> Result<()>;
    fn identity(&self) -> &Path;
}
#[derive(Debug)]
enum Original {
    Pwm { enable: String, speed: String },
    ThinkPad { level: String },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ownership {
    Released,
    Active,
    Protective,
}
/// Read-only construction followed by explicit activation. The object remembers
/// the original state before the first mutation, even if activation fails midway.
#[derive(Debug)]
pub struct FileFan {
    path: PathBuf,
    original: Original,
    ownership: Ownership,
    watchdog: u32,
    pub(crate) fault_command: FanCommand,
    inode: (u64, u64),
}
fn write(path: &Path, value: &str) -> Result<()> {
    // Never create missing sysfs/proc files; each command uses a fresh descriptor.
    let mut file = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
        .with_context(|| format!("opening {} for fan control", path.display()))?;
    file.write_all(value.as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    file.flush()?;
    Ok(())
}
impl FileFan {
    pub fn probe(config: &FanConfig, watchdog: u32, fault_command: FanCommand) -> Result<Self> {
        Self::probe_selector(&config.selector()?, watchdog, fault_command)
    }
    pub fn probe_selector(
        selector: &FanSelector,
        watchdog: u32,
        fault_command: FanCommand,
    ) -> Result<Self> {
        ensure!(
            matches!(fault_command, FanCommand::ThinkPad(_)) == selector.thinkpad(),
            "fault command backend mismatch"
        );
        FanCommand::compile(&fault_command.speed(), selector.thinkpad())?;
        let (path, original) = if let FanSelector::Hwmon(s) = selector {
            let path = resolve_hwmon(
                &s.path,
                s.name.as_deref(),
                s.model.as_deref(),
                s.indices.as_deref(),
                true,
            )?
            .remove(0);
            let enable = fs::read_to_string(enable_path(&path))?.trim().to_owned();
            let speed = fs::read_to_string(&path)?.trim().to_owned();
            let _: u32 = enable.parse().context("invalid PWM enable state")?;
            ensure!(
                speed.parse::<u16>().is_ok_and(|n| n <= 255),
                "invalid original PWM speed"
            );
            (path, Original::Pwm { enable, speed })
        } else {
            let FanSelector::ThinkPad(path) = selector else {
                unreachable!()
            };
            let path = fs::canonicalize(path)?;
            let text = fs::read_to_string(&path)?;
            ensure!(
                text.lines()
                    .any(|l| l.starts_with("commands:") && l.contains("level <level>")),
                "ThinkPad fan control is unavailable; enable thinkpad_acpi fan_control=1"
            );
            let raw = text
                .lines()
                .find_map(|l| l.strip_prefix("level:"))
                .context("ThinkPad fan has no level field")?
                .trim();
            let level = Speed::Mode(raw.to_owned()).normalized(true)?.command(true);
            (path, Original::ThinkPad { level })
        };
        let metadata = fs::metadata(&path)?;
        let inode = (metadata.dev(), metadata.ino());
        Ok(Self {
            path,
            original,
            ownership: Ownership::Released,
            watchdog,
            fault_command,
            inode,
        })
    }
    pub fn apply_command(&mut self, command: FanCommand) -> Result<()> {
        ensure!(
            self.still_present(),
            "fan device disappeared or was replaced: {}",
            self.path.display()
        );
        ensure!(
            matches!(command, FanCommand::ThinkPad(_)) == self.thinkpad(),
            "command backend mismatch"
        );
        if let FanCommand::ThinkPad(crate::policy::control::ThinkPadCommand::Level(level)) = command
        {
            ensure!(level <= 7, "ThinkPad level out of range");
        }
        self.ownership = Ownership::Active;
        if self.thinkpad() {
            write(&self.path, &format!("watchdog {}", self.watchdog))?;
        } else {
            // Reassert mode after resume or an external firmware reset.
            write(&enable_path(&self.path), "1")?;
        }
        write(&self.path, &command.to_string())
    }
    fn thinkpad(&self) -> bool {
        matches!(self.original, Original::ThinkPad { .. })
    }
    pub(crate) fn hardware_identity(&self) -> (u64, u64) {
        self.inode
    }
    pub fn still_present(&self) -> bool {
        fs::metadata(&self.path).is_ok_and(|m| (m.dev(), m.ino()) == self.inode)
    }
    /// A removed device no longer has a state we can restore. Never restore
    /// its saved mode to a different device that reused the same sysfs path.
    pub fn detach(&mut self) {
        self.ownership = Ownership::Released;
    }
    pub fn healthy(&mut self) {
        if self.ownership != Ownership::Released {
            self.ownership = Ownership::Active;
        }
    }
}
fn enable_path(path: &Path) -> PathBuf {
    let mut p = path.as_os_str().to_os_string();
    p.push("_enable");
    PathBuf::from(p)
}
impl FanDevice for FileFan {
    fn identity(&self) -> &Path {
        &self.path
    }
    fn apply(&mut self, speed: &Speed) -> Result<()> {
        self.apply_command(FanCommand::compile(speed, self.thinkpad())?)
    }
    fn restore(&mut self) -> Result<()> {
        if self.ownership == Ownership::Released {
            return Ok(());
        }
        if !self.still_present() {
            self.ownership = Ownership::Released;
            return Ok(());
        }
        match &self.original {
            Original::Pwm { enable, speed } => {
                // If original mode was manual, restore its duty cycle first.
                if enable == "1" {
                    write(&self.path, speed)?;
                }
                write(&enable_path(&self.path), enable)?;
            }
            Original::ThinkPad { level } => {
                write(&self.path, level)?;
                write(&self.path, "watchdog 0")?;
            }
        }
        self.ownership = Ownership::Released;
        Ok(())
    }
    fn emergency(&mut self) -> Result<()> {
        self.ownership = Ownership::Protective;
        ensure!(
            self.still_present(),
            "emergency target is no longer present"
        );
        self.ownership = Ownership::Protective;
        if self.thinkpad() {
            // A broken watchdog command must not prevent a speed command.
            if let Err(error) = write(&self.path, &format!("watchdog {}", self.watchdog)) {
                tracing::error!(path = %self.path.display(), error = %format_args!("{error:#}"), "emergency watchdog setup failed");
            }
            write(&self.path, &self.fault_command.to_string())
                .or_else(|_| write(&self.path, "level auto"))
        } else {
            let mode = write(&enable_path(&self.path), "1");
            let speed = write(&self.path, &self.fault_command.to_string());
            if mode.is_ok() && speed.is_ok() {
                return Ok(());
            }
            // Standard hwmon mode 0 disables PWM regulation (full speed).
            write(&enable_path(&self.path), "0")
                .context("both configured fault speed and unregulated fan mode failed")
        }
    }
}
impl Drop for FileFan {
    fn drop(&mut self) {
        if self.ownership == Ownership::Released {
            return;
        }
        let result = if self.ownership == Ownership::Protective || std::thread::panicking() {
            self.emergency()
        } else {
            self.restore()
        };
        if let Err(error) = result {
            tracing::error!(path = %self.path.display(), error = %format_args!("{error:#}"), "fan cleanup failed");
        }
    }
}
