//! hwmon selector resolution, identity validation and cached reads.
use crate::config::HwmonSelector;
use anyhow::{Context, Result, ensure};
use std::{
    collections::HashSet,
    fs::{self, File},
    io::Read,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// Bounded traversal handles /sys/class symlinks without following device cycles.
/// Ambiguous matches are errors; traversal order never selects the hardware.
pub fn resolve_hwmon(
    root: &Path,
    name: Option<&str>,
    model: Option<&str>,
    indices: Option<&[usize]>,
    fan: bool,
) -> Result<Vec<PathBuf>> {
    let _measure = crate::metrics::Measure::start(crate::metrics::Operation::HwmonDiscovery);
    let Some(indices) = indices else {
        ensure!(
            root.is_file(),
            "{} is not a device file; directory lookup requires indices",
            root.display()
        );
        return Ok(vec![fs::canonicalize(root)?]);
    };
    let file_name = |i| {
        if fan {
            format!("pwm{i}")
        } else {
            format!("temp{i}_input")
        }
    };
    let mut queue = vec![(root.to_path_buf(), 0)];
    let mut visited = HashSet::new();
    let mut candidates = Vec::new();
    while let Some((directory, depth)) = queue.pop() {
        let Ok(real) = fs::canonicalize(&directory) else {
            continue;
        };
        if !visited.insert(real.clone()) {
            continue;
        }
        let matches = |p: &Path, expected: Option<&str>| {
            expected.is_none_or(|s| fs::read_to_string(p).is_ok_and(|v| v.trim() == s))
        };
        if matches(&directory.join("name"), name)
            && matches(&directory.join("device/model"), model)
            && indices
                .iter()
                .all(|i| directory.join(file_name(i)).is_file())
        {
            candidates.push(real);
            continue;
        }
        if depth < 4
            && let Ok(entries) = fs::read_dir(&directory)
        {
            for entry in entries.flatten() {
                let path = entry.path();
                // Do not recurse through device/subsystem/power backlinks.
                let label = entry.file_name();
                let label = label.to_string_lossy();
                if path.is_dir()
                    && (label == "hwmon"
                        || label.starts_with("hwmon")
                        || depth == 0
                            && label != "device"
                            && label != "subsystem"
                            && label != "power")
                {
                    queue.push((path, depth + 1));
                }
            }
        }
    }
    ensure!(
        candidates.len() == 1,
        "hwmon lookup under {} matched {} interfaces (expected exactly one)",
        root.display(),
        candidates.len()
    );
    indices
        .iter()
        .map(|i| {
            fs::canonicalize(candidates[0].join(file_name(i))).context("resolving hwmon channel")
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq)]
struct Channel {
    path: PathBuf,
    device: u64,
    inode: u64,
}
pub struct HwmonSensor {
    selector: HwmonSelector,
    channels: Vec<Channel>,
    root_stamp: Option<(u64, u64, i64, i64)>,
    last_scan: Option<Instant>,
    pub generation: u64,
}
impl HwmonSensor {
    pub fn new(selector: HwmonSelector) -> Self {
        Self {
            selector,
            channels: Vec::new(),
            root_stamp: None,
            last_scan: None,
            generation: 0,
        }
    }
    pub fn invalidate(&mut self) {
        self.channels.clear();
        self.last_scan = None;
    }
    fn stamp(&self) -> Option<(u64, u64, i64, i64)> {
        fs::metadata(&self.selector.path)
            .ok()
            .map(|m| (m.dev(), m.ino(), m.mtime(), m.mtime_nsec()))
    }
    fn matches(&self) -> bool {
        self.channels.iter().all(|c| {
            let Some(dir) = c.path.parent() else {
                return false;
            };
            [
                ("name", &self.selector.name),
                ("device/model", &self.selector.model),
            ]
            .iter()
            .all(|(file, expected)| {
                expected
                    .as_ref()
                    .is_none_or(|s| fs::read_to_string(dir.join(file)).is_ok_and(|v| v.trim() == s))
            }) && fs::metadata(&c.path).is_ok_and(|m| (m.dev(), m.ino()) == (c.device, c.inode))
        })
    }
    fn resolve(&mut self) -> Result<()> {
        let paths = resolve_hwmon(
            &self.selector.path,
            self.selector.name.as_deref(),
            self.selector.model.as_deref(),
            self.selector.indices.as_deref(),
            false,
        )?;
        let channels = paths
            .into_iter()
            .map(|path| {
                let m = fs::metadata(&path)?;
                Ok(Channel {
                    path,
                    device: m.dev(),
                    inode: m.ino(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        if channels != self.channels {
            self.generation = self.generation.wrapping_add(1);
        }
        self.channels = channels;
        self.root_stamp = self.stamp();
        self.last_scan = Some(Instant::now());
        Ok(())
    }
    pub fn read(&mut self) -> Result<Vec<Option<f64>>> {
        // Full rediscovery catches newly ambiguous selectors at least every 60s.
        let rescan = self.channels.is_empty()
            || self
                .last_scan
                .is_none_or(|t| t.elapsed() >= Duration::from_secs(60))
            || (self.selector.indices.is_some() && self.root_stamp != self.stamp())
            || !self.matches();
        if rescan && let Err(e) = self.resolve() {
            self.invalidate();
            return Err(e);
        }
        let result = self
            .channels
            .iter()
            .map(|c| {
                let mut file =
                    File::open(&c.path).with_context(|| format!("reading {}", c.path.display()))?;
                let m = file.metadata()?;
                ensure!(
                    (m.dev(), m.ino()) == (c.device, c.inode),
                    "hwmon identity changed while opening"
                );
                let mut text = String::new();
                file.read_to_string(&mut text)?;
                ensure!(
                    fs::metadata(&c.path).is_ok_and(|m| (m.dev(), m.ino()) == (c.device, c.inode)),
                    "hwmon identity changed while reading"
                );
                Ok(Some(
                    text.trim()
                        .parse::<f64>()
                        .context("invalid hwmon temperature")?
                        / 1000.0,
                ))
            })
            .collect();
        if Result::is_err(&result) {
            self.invalidate();
        }
        result
    }
}
