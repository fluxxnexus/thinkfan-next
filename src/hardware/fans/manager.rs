use super::{FanDevice, FileFan};
use crate::{
    config::FanPlan,
    policy::control::{FanCommand, ThinkPadCommand},
};
use anyhow::{Context, Result, ensure};
use std::{
    collections::HashSet,
    path::PathBuf,
    sync::mpsc::{self, Receiver, SyncSender},
    time::{Duration, Instant},
};
struct Slot {
    plan: FanPlan,
    device: Option<FileFan>,
    errors: u32,
    probe: ProbeState,
    last_write: Option<(FanCommand, bool, Instant)>,
}
enum ProbeState {
    Idle(Instant),
    Running { since: Instant, epoch: u64 },
    TimedOut { epoch: u64 },
}
struct ProbeResult {
    index: usize,
    epoch: u64,
    result: Result<FileFan>,
}
pub struct FanManager {
    slots: Vec<Slot>,
    watchdog: u32,
    dry_run: bool,
    epoch: u64,
    probes: Receiver<ProbeResult>,
    probe_sender: SyncSender<ProbeResult>,
}
#[derive(Debug, Default)]
pub struct ExecutionReport {
    pub protective: bool,
    pub fatal: Option<anyhow::Error>,
}
#[derive(Debug)]
pub struct ReloadPlan {
    pub retained: Vec<(usize, usize)>,
    pub added: Vec<usize>,
    pub removed: Vec<usize>,
    pub released: Vec<usize>,
}
impl FanManager {
    pub fn prepare(plans: Vec<FanPlan>, watchdog: u32, dry_run: bool) -> Result<Self> {
        let (probe_sender, probes) = mpsc::sync_channel(plans.len().max(1));
        let mut slots = Vec::new();
        for plan in plans {
            let device = match FileFan::probe_selector(
                &plan.selector,
                watchdog,
                plan.fault_command(),
            ) {
                Ok(device) => Some(device),
                Err(error) if plan.optional || plan.max_errors > 0 => {
                    tracing::warn!(error = %format_args!("{error:#}"), "fan unavailable during preparation");
                    None
                }
                Err(error) => return Err(error).context("probing fan"),
            };
            slots.push(Slot {
                plan,
                device,
                errors: 0,
                probe: ProbeState::Idle(Instant::now()),
                last_write: None,
            });
        }
        let manager = Self {
            slots,
            watchdog,
            dry_run,
            epoch: 0,
            probes,
            probe_sender,
        };
        manager.unique()?;
        Ok(manager)
    }
    fn unique(&self) -> Result<()> {
        let mut identities = HashSet::new();
        for slot in &self.slots {
            if let Some(device) = &slot.device {
                ensure!(
                    identities.insert(device.hardware_identity()),
                    "multiple entries resolve to the same fan: {}",
                    device.identity().display()
                );
            }
        }
        Ok(())
    }
    pub fn check_hardware(&self) -> Result<()> {
        for slot in &self.slots {
            ensure!(
                slot.plan.optional || slot.device.is_some(),
                "required fan unavailable"
            );
        }
        Ok(())
    }
    pub fn refresh(&mut self) -> Result<bool> {
        let mut uncertain = false;
        for slot in &mut self.slots {
            if slot.device.as_ref().is_some_and(|d| !d.still_present()) {
                slot.device.as_mut().unwrap().detach();
                slot.device = None;
            }
            if slot.device.is_none() {
                match FileFan::probe_selector(
                    &slot.plan.selector,
                    self.watchdog,
                    slot.plan.fault_command(),
                ) {
                    Ok(device) => slot.device = Some(device),
                    Err(error) => {
                        slot.errors = slot.errors.saturating_add(1);
                        if !slot.plan.optional {
                            uncertain = true;
                            ensure!(
                                slot.errors <= slot.plan.max_errors,
                                "required fan unavailable: {error:#}"
                            );
                        }
                    }
                }
            }
        }
        self.unique()?;
        Ok(uncertain)
    }
    pub(crate) fn refresh_async(
        &mut self,
        workers: &crate::hardware::ReadWorkers,
        now: Instant,
        interval: Duration,
    ) -> Result<(bool, bool)> {
        let mut changed = false;
        let mut fatal = None;
        while let Ok(done) = self.probes.try_recv() {
            let slot = &mut self.slots[done.index];
            let timed_out =
                matches!(slot.probe, ProbeState::TimedOut { epoch } if epoch == done.epoch);
            slot.probe = ProbeState::Idle(now + interval);
            if done.epoch != self.epoch {
                continue;
            }
            changed = true;
            match done.result.and_then(|d| {
                ensure!(d.still_present(), "fan identity changed during probe");
                Ok(d)
            }) {
                Ok(device) => {
                    slot.device = Some(device);
                    slot.last_write = None;
                }
                Err(error) => {
                    if !timed_out {
                        slot.errors = slot.errors.saturating_add(1);
                    }
                    tracing::warn!(fan = done.index, error = %format_args!("{error:#}"), "fan probe failed");
                    if !slot.plan.optional && slot.errors > slot.plan.max_errors {
                        fatal = Some(error.context("required fan unavailable"));
                    }
                }
            }
        }
        for (index, slot) in self.slots.iter_mut().enumerate() {
            if slot.device.as_ref().is_some_and(|d| !d.still_present()) {
                slot.device.as_mut().unwrap().detach();
                slot.device = None;
                slot.last_write = None;
                slot.probe = ProbeState::Idle(now);
                changed = true;
            }
            if matches!(slot.probe, ProbeState::Running { since, epoch } if epoch == self.epoch && now.saturating_duration_since(since) >= interval * 2)
            {
                slot.probe = ProbeState::TimedOut { epoch: self.epoch };
                slot.errors = slot.errors.saturating_add(1);
                changed = true;
                if !slot.plan.optional && slot.errors > slot.plan.max_errors {
                    fatal = Some(anyhow::anyhow!(
                        "fan {index} probe timed out; error budget exceeded"
                    ));
                }
            }
            if slot.device.is_none() && matches!(slot.probe, ProbeState::Idle(due) if now >= due) {
                let plan = slot.plan.clone();
                let watchdog = self.watchdog;
                let sender = self.probe_sender.clone();
                let epoch = self.epoch;
                if workers.submit(crate::hardware::WorkClass::Probe, || {
                    Box::new(move || {
                        let _measure =
                            crate::metrics::Measure::start(crate::metrics::Operation::FanProbe);
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            FileFan::probe_selector(&plan.selector, watchdog, plan.fault_command())
                        }))
                        .unwrap_or_else(|_| Err(anyhow::anyhow!("fan probe panicked")));
                        let _ = sender.send(ProbeResult {
                            index,
                            epoch,
                            result,
                        });
                    })
                })? {
                    slot.probe = ProbeState::Running {
                        since: now,
                        epoch: self.epoch,
                    };
                }
            }
        }
        self.unique()?;
        if let Some(error) = fatal {
            return Err(error);
        }
        Ok((
            self.slots
                .iter()
                .any(|s| !s.plan.optional && s.device.is_none()),
            changed,
        ))
    }
    pub fn apply(&mut self, commands: &[FanCommand], protective: bool) -> ExecutionReport {
        self.apply_scheduled(commands, protective, Instant::now(), Duration::ZERO)
    }
    pub(crate) fn apply_scheduled(
        &mut self,
        commands: &[FanCommand],
        protective: bool,
        now: Instant,
        maintenance: Duration,
    ) -> ExecutionReport {
        let mut report = ExecutionReport {
            protective,
            fatal: None,
        };
        if commands.len() != self.slots.len() {
            report.fatal = Some(anyhow::anyhow!("fan command layout mismatch"));
            report.protective = true;
            self.emergency();
            return report;
        }
        if self.dry_run {
            return report;
        }
        for (index, (slot, &command)) in self.slots.iter_mut().zip(commands).enumerate() {
            if let Some(device) = &mut slot.device {
                let target = if report.protective {
                    slot.plan.fault_command()
                } else {
                    command
                };
                if slot.last_write.is_some_and(|(last, mode, at)| {
                    last == target
                        && mode == report.protective
                        && now.saturating_duration_since(at) < maintenance
                }) {
                    continue;
                }
                let _measure = crate::metrics::Measure::start(crate::metrics::Operation::FanApply);
                let result = if report.protective {
                    device.emergency()
                } else {
                    device.apply_command(command)
                };
                match result {
                    Ok(()) => {
                        slot.errors = 0;
                        slot.last_write = Some((target, report.protective, now));
                        if !report.protective {
                            device.healthy();
                        }
                    }
                    Err(error) => {
                        slot.last_write = None;
                        slot.errors = slot.errors.saturating_add(1);
                        tracing::error!(fan = index, errors = slot.errors, error = %format_args!("{error:#}"), "fan write failed");
                        if !slot.plan.optional {
                            report.protective = true;
                            if slot.errors > slot.plan.max_errors {
                                report.fatal = Some(error.context("fan error budget exceeded"));
                            }
                        }
                    }
                }
            }
        }
        // Retry every fan in this cycle, including those written before the failure.
        if report.protective && !protective {
            self.emergency();
        }
        report
    }
    pub fn emergency(&mut self) -> Vec<PathBuf> {
        let mut failed = Vec::new();
        if self.dry_run {
            return failed;
        }
        for slot in &mut self.slots {
            slot.last_write = None;
            if let Some(device) = &mut slot.device
                && let Err(error) = device.emergency()
            {
                tracing::error!(path = %device.identity().display(), error = %format_args!("{error:#}"), "emergency fan command failed");
                failed.push(device.identity().to_owned());
            }
        }
        failed
    }
    pub fn restore(&mut self) -> Result<()> {
        self.epoch = self.epoch.wrapping_add(1);
        let mut errors = Vec::new();
        for slot in &mut self.slots {
            slot.last_write = None;
            if let Some(device) = &mut slot.device
                && let Err(error) = device.restore()
            {
                errors.push(format!("{error:#}"));
            }
        }
        ensure!(
            errors.is_empty(),
            "restoring fan state: {}",
            errors.join("; ")
        );
        Ok(())
    }
    pub fn depulse_start(&mut self) -> Result<()> {
        if self.dry_run {
            return Ok(());
        }
        for slot in &mut self.slots {
            slot.last_write = None;
            if slot.plan.selector.thinkpad()
                && let Some(device) = &mut slot.device
            {
                device.apply_command(FanCommand::ThinkPad(ThinkPadCommand::Disengaged))?;
            }
        }
        Ok(())
    }
    pub fn depulse_end(&mut self, commands: &[FanCommand]) -> Result<()> {
        if self.dry_run {
            return Ok(());
        }
        for (slot, &command) in self.slots.iter_mut().zip(commands) {
            slot.last_write = None;
            if slot.plan.selector.thinkpad()
                && let Some(device) = &mut slot.device
            {
                device.apply_command(command)?;
            }
        }
        Ok(())
    }
    pub fn reload_plan(&self, next: &Self) -> ReloadPlan {
        let mut retained = Vec::new();
        let mut added = Vec::new();
        for (new_index, new) in next.slots.iter().enumerate() {
            let old = new.device.as_ref().and_then(|probe| {
                self.slots.iter().position(|s| {
                    s.device.as_ref().is_some_and(|d| {
                        d.still_present()
                            && probe.still_present()
                            && d.identity() == probe.identity()
                            && d.hardware_identity() == probe.hardware_identity()
                    })
                })
            });
            match old {
                Some(old_index) => retained.push((old_index, new_index)),
                None => added.push(new_index),
            }
        }
        let removed = (0..self.slots.len())
            .filter(|i| !retained.iter().any(|(old, _)| old == i))
            .collect();
        ReloadPlan {
            retained,
            added,
            removed,
            released: Vec::new(),
        }
    }
    pub fn commit_reload(&mut self, next: &mut Self) -> Result<()> {
        ensure!(
            next.slots
                .iter()
                .all(|s| s.device.as_ref().is_none_or(FileFan::still_present)),
            "new fan identity changed during reload preparation"
        );
        let mut plan = self.reload_plan(next);
        for &index in &plan.removed {
            if let Some(device) = &mut self.slots[index].device {
                if let Err(error) = device.restore() {
                    let failed = self.emergency();
                    tracing::error!(
                        ?plan,
                        ?failed,
                        "reload partial failure; old fan plan reclaimed"
                    );
                    return Err(error).with_context(|| format!("reload partially failed; reclaimed old fans, failed protective targets: {failed:?}"));
                }
                plan.released.push(index);
            }
        }
        for &(old, new) in &plan.retained {
            let mut device = self.slots[old].device.take();
            if let Some(device) = &mut device {
                device.fault_command = next.slots[new].plan.fault_command();
            }
            next.slots[new].device = device;
        }
        tracing::debug!(?plan, "reload hardware operations completed");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::fs;
    #[test]
    fn unchanged_commands_skip_writes_but_maintenance_reasserts_mode() {
        let dir = tempfile::tempdir().unwrap();
        let pwm = dir.path().join("pwm");
        let enable = dir.path().join("pwm_enable");
        fs::write(&pwm, "73").unwrap();
        fs::write(&enable, "2").unwrap();
        let yaml = format!(
            "sensors: [{{hwmon: /unused}}]\nfans: [{{hwmon: {}}}]\nlevels: [[0, 0, 50], [180, 45, 120]]",
            pwm.display()
        );
        let plan = Config::parse(&yaml).unwrap().compile(&[1]).unwrap();
        let mut manager = FanManager::prepare(plan.fans, 120, false).unwrap();
        let now = Instant::now();
        let every = Duration::from_secs(5);
        assert!(
            manager
                .apply_scheduled(&[FanCommand::Pwm(0)], false, now, every)
                .fatal
                .is_none()
        );
        fs::write(&pwm, "99").unwrap();
        fs::write(&enable, "2").unwrap();
        manager.apply_scheduled(
            &[FanCommand::Pwm(0)],
            false,
            now + Duration::from_secs(1),
            every,
        );
        assert_eq!(fs::read_to_string(&pwm).unwrap(), "99");
        manager.apply_scheduled(&[FanCommand::Pwm(0)], false, now + every, Duration::ZERO);
        assert_eq!(fs::read_to_string(&pwm).unwrap(), "0");
        assert_eq!(fs::read_to_string(&enable).unwrap(), "1");
        let report = manager.apply_scheduled(&[FanCommand::Pwm(180)], false, now + every, every);
        assert!(report.fatal.is_none());
        // Same command, different protective mode: cached duty must not hide the transition.
        fs::write(&enable, "2").unwrap();
        let report = manager.apply_scheduled(&[FanCommand::Pwm(180)], true, now + every, every);
        assert!(report.fatal.is_none());
        assert!(report.protective);
        assert_eq!(fs::read_to_string(&enable).unwrap(), "1");
        assert_eq!(fs::read_to_string(&pwm).unwrap(), "180");
        manager.restore().unwrap();
        manager.apply_scheduled(&[FanCommand::Pwm(180)], true, now + every, every);
        assert_eq!(fs::read_to_string(&enable).unwrap(), "1");
    }
}
