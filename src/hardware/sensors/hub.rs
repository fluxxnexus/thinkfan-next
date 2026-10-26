use super::{ConfiguredSensor, SensorDevice};
use crate::{
    config::SensorPlan,
    policy::sample::{Sample, SampleState},
};
use anyhow::{Context, Result, ensure};
use std::{
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, SyncSender},
    },
    time::{Duration, Instant},
};
fn backend_name(source: &crate::config::SensorSource) -> &'static str {
    use crate::config::SensorSource;
    match source {
        SensorSource::Hwmon(_) => "hwmon",
        SensorSource::ThinkPad { .. } => "thinkpad",
        SensorSource::LmSensors { .. } => "lm-sensors",
        SensorSource::Nvml { .. } => "nvml",
        SensorSource::AtaSmart { .. } => "smart",
    }
}
fn device_label(source: &crate::config::SensorSource) -> String {
    use crate::config::SensorSource;
    match source {
        SensorSource::Hwmon(selector) => selector.path.display().to_string(),
        SensorSource::ThinkPad { path, .. } | SensorSource::AtaSmart { path } => {
            path.display().to_string()
        }
        SensorSource::LmSensors { chip, .. } => chip.clone(),
        SensorSource::Nvml { bus } => bus.clone(),
    }
}
enum Acquisition {
    Idle(Box<ConfiguredSensor>),
    InFlight { started: Duration, epoch: u64 },
    TimedOut { started: Duration, epoch: u64 },
}
impl Acquisition {
    fn device(&self) -> Option<&ConfiguredSensor> {
        match self {
            Self::Idle(device) => Some(device),
            _ => None,
        }
    }
    fn device_mut(&mut self) -> Option<&mut ConfiguredSensor> {
        match self {
            Self::Idle(device) => Some(device),
            _ => None,
        }
    }
    fn take(&mut self, started: Duration, epoch: u64) -> Box<ConfiguredSensor> {
        match std::mem::replace(self, Self::InFlight { started, epoch }) {
            Self::Idle(device) => device,
            _ => unreachable!("only idle devices can be dispatched"),
        }
    }
}
struct Slot {
    acquisition: Acquisition,
    plan: SensorPlan,
    errors: u32,
    sequence: u64,
    next_due: Duration,
    waiting_capacity: bool,
}
pub struct SensorHub {
    slots: Vec<Slot>,
    pub samples: Vec<Sample>,
    pub uncertain: bool,
    epoch: u64,
    cursor: usize,
    results: Receiver<Completion>,
    sender: SyncSender<Completion>,
    publication: Arc<Mutex<()>>,
}
struct Completion {
    index: usize,
    epoch: u64,
    device: Box<ConfiguredSensor>,
    captured_at: Duration,
    result: Result<Vec<Option<f64>>>,
}
impl SensorHub {
    pub fn new(devices: Vec<ConfiguredSensor>, plans: Vec<SensorPlan>, width: usize) -> Self {
        let (sender, results) = mpsc::sync_channel(devices.len().max(1));
        Self {
            epoch: 0,
            cursor: 0,
            results,
            sender,
            publication: Arc::new(Mutex::new(())),
            slots: devices
                .into_iter()
                .zip(plans)
                .map(|(device, plan)| Slot {
                    acquisition: Acquisition::Idle(Box::new(device)),
                    plan,
                    errors: 0,
                    sequence: 0,
                    next_due: Duration::ZERO,
                    waiting_capacity: false,
                })
                .collect(),
            samples: vec![Sample::default(); width],
            uncertain: false,
        }
    }
    pub fn collect(
        &mut self,
        at: Option<Duration>,
        origin: Instant,
        interval: Duration,
    ) -> Result<Duration> {
        ensure!(
            self.slots.iter().all(|s| s.acquisition.device().is_some()),
            "synchronous collection during asynchronous acquisition"
        );
        let now = at.unwrap_or_else(|| origin.elapsed());
        let lm = self
            .slots
            .iter()
            .any(|s| {
                s.acquisition.device().unwrap().is_lm()
                    && (s.plan.interval.is_none() || now >= s.next_due)
            })
            .then(crate::hardware::backends::LmSession::open);
        let mut fatal = None;
        for index in 0..self.slots.len() {
            let slot = &mut self.slots[index];
            if slot.plan.interval.is_some() && now < slot.next_due {
                continue;
            }
            let mut device = slot.acquisition.take(now, self.epoch);
            let result = read_device(&mut device, lm.as_ref());
            let captured_at = at.unwrap_or_else(|| origin.elapsed());
            if let Err(error) = self.accept(
                Completion {
                    index,
                    epoch: self.epoch,
                    device,
                    captured_at,
                    result,
                },
                interval,
                false,
            ) {
                fatal = Some(error);
            }
        }
        self.update_uncertain();
        if let Some(error) = fatal {
            return Err(error);
        }
        Ok(at.unwrap_or_else(|| origin.elapsed()))
    }
    /// Submit only due, idle devices. All libsensors entries due together share
    /// one context, created and destroyed exclusively inside the worker.
    pub(crate) fn schedule(
        &mut self,
        workers: &crate::hardware::ReadWorkers,
        origin: Instant,
    ) -> Result<()> {
        let now = origin.elapsed();
        let mut groups: Vec<Vec<usize>> = Vec::new();
        let mut lm_group = None;
        for offset in 0..self.slots.len() {
            let i = (self.cursor + offset) % self.slots.len();
            let slot = &self.slots[i];
            if let Some(device) = slot.acquisition.device()
                && now >= slot.next_due
            {
                if device.is_lm() {
                    let group = *lm_group.get_or_insert_with(|| {
                        groups.push(Vec::new());
                        groups.len() - 1
                    });
                    groups[group].push(i);
                } else {
                    groups.push(vec![i]);
                }
            }
        }
        for indices in groups {
            let sender = self.sender.clone();
            let publication = self.publication.clone();
            let epoch = self.epoch;
            let next_cursor = (indices.last().unwrap() + 1) % self.slots.len();
            let class = match self.slots[indices[0]].plan.source {
                crate::config::SensorSource::Nvml { .. } => crate::hardware::WorkClass::Gpu,
                crate::config::SensorSource::AtaSmart { .. } => crate::hardware::WorkClass::Disk,
                _ => crate::hardware::WorkClass::Fast,
            };
            let wake = workers.waker();
            let submitted = workers.submit(class, || {
                let devices = indices
                    .iter()
                    .copied()
                    .map(|i| {
                        let slot = &mut self.slots[i];
                        crate::metrics::record(
                            crate::metrics::Operation::DispatchDelay,
                            now.saturating_sub(slot.next_due),
                        );
                        (i, slot.acquisition.take(now, epoch))
                    })
                    .collect::<Vec<_>>();
                Box::new(move || {
                    // Publish each entry before reading the next one. A slow later
                    // channel cannot hide an already completed temperature.
                    let mut lm = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        devices
                            .iter()
                            .any(|(_, d)| d.is_lm())
                            .then(crate::hardware::backends::LmSession::open)
                    }))
                    .unwrap_or_else(|_| {
                        Some(Err(anyhow::anyhow!("libsensors initialization panicked")))
                    });
                    let mut devices = devices.into_iter().peekable();
                    while let Some((index, mut device)) = devices.next() {
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            read_device(&mut device, lm.as_ref())
                        }))
                        .unwrap_or_else(|_| Err(anyhow::anyhow!("sensor backend panicked")));
                        // The final completion may let --once exit immediately.
                        // Release the session first, without delaying earlier
                        // channels. Cleanup remains subject to the read timeout.
                        if devices.peek().is_none() {
                            drop(lm.take());
                        }
                        // Timestamp and publication share the timeout check lock.
                        // No hardware access occurs while this lock is held.
                        let published = {
                            let _guard = publication.lock().unwrap();
                            sender.send(Completion {
                                index,
                                epoch,
                                device,
                                captured_at: origin.elapsed(),
                                result,
                            })
                        };
                        if published.is_err() {
                            break;
                        }
                        if let Err(error) = wake.wake() {
                            tracing::error!(%error, "sensor result wake failed");
                        }
                    }
                })
            })?;
            for index in indices {
                self.slots[index].waiting_capacity = !submitted;
            }
            if !submitted {
                continue;
            }
            self.cursor = next_cursor;
        }
        Ok(())
    }
    pub(crate) fn check_timeouts(&mut self, now: Duration, interval: Duration) -> Result<()> {
        // Drain and inspect deadlines atomically with respect to publication. A
        // second drain without this lock would leave the same race window open.
        // The channel has one place per sensor and each sensor has at most one
        // outstanding result, so publishing while holding the lock cannot block.
        let publication = self.publication.clone();
        let _guard = publication.lock().unwrap();
        self.drain(interval, now)?;
        for (index, slot) in self.slots.iter_mut().enumerate() {
            if matches!(slot.acquisition, Acquisition::InFlight { started, epoch } if epoch == self.epoch && now.saturating_sub(started) >= slot.plan.read_timeout.unwrap_or(interval * 2))
            {
                let Acquisition::InFlight { started, .. } = slot.acquisition else {
                    unreachable!()
                };
                slot.acquisition = Acquisition::TimedOut {
                    started,
                    epoch: self.epoch,
                };
                slot.errors = slot.errors.saturating_add(1);
                crate::metrics::record(crate::metrics::Operation::ReadTimeout, Duration::ZERO);
                tracing::warn!(
                    sensor = index,
                    backend = backend_name(&slot.plan.source), device = %device_label(&slot.plan.source),
                    timeout_ms = slot.plan.read_timeout.unwrap_or(interval * 2).as_millis() as u64,
                    elapsed_ms = now.saturating_sub(started).as_millis() as u64,
                    errors = slot.errors,
                    max_errors = slot.plan.max_errors,
                    optional = slot.plan.optional,
                    "sensor acquisition timed out; worker remains occupied"
                );
                if !slot.plan.optional {
                    self.uncertain = true;
                    ensure!(
                        slot.errors <= slot.plan.max_errors,
                        "sensor {index} acquisition timed out; error budget exceeded"
                    );
                }
            }
        }
        Ok(())
    }
    fn update_uncertain(&mut self) {
        self.uncertain = self
            .slots
            .iter()
            .any(|s| !s.plan.optional && (s.errors > 0 || s.sequence == 0));
    }
    /// Both synchronous checks and worker completions use the same sample/error semantics.
    fn accept(
        &mut self,
        mut done: Completion,
        interval: Duration,
        enforce_timeout: bool,
    ) -> Result<bool> {
        let mut late_completion = false;
        if enforce_timeout && done.epoch == self.epoch {
            let slot = &self.slots[done.index];
            let late = match slot.acquisition {
                Acquisition::InFlight { started, .. } => {
                    done.captured_at.saturating_sub(started)
                        >= slot.plan.read_timeout.unwrap_or(interval * 2)
                }
                Acquisition::TimedOut { epoch, .. } => epoch == done.epoch,
                Acquisition::Idle(_) => false,
            };
            if late {
                late_completion = true;
                if !matches!(slot.acquisition, Acquisition::TimedOut { .. }) {
                    crate::metrics::record(crate::metrics::Operation::ReadTimeout, Duration::ZERO);
                }
                let started = match slot.acquisition {
                    Acquisition::InFlight { started, .. }
                    | Acquisition::TimedOut { started, .. } => started,
                    Acquisition::Idle(_) => unreachable!(),
                };
                tracing::warn!(
                    sensor = done.index,
                    backend = backend_name(&slot.plan.source), device = %device_label(&slot.plan.source),
                    timeout_ms = slot.plan.read_timeout.unwrap_or(interval * 2).as_millis() as u64,
                    elapsed_ms = done.captured_at.saturating_sub(started).as_millis() as u64,
                    errors = slot.errors.saturating_add(u32::from(!matches!(slot.acquisition, Acquisition::TimedOut { .. }))),
                    max_errors = slot.plan.max_errors,
                    read_succeeded = done.result.is_ok(),
                    original_error = ?done.result.as_ref().err().map(|error| format!("{error:#}")),
                    "sensor acquisition completed after its deadline; result discarded"
                );
                done.result = match done.result {
                    Ok(_) => Err(anyhow::anyhow!(
                        "sensor acquisition completed after its deadline"
                    )),
                    Err(error) => {
                        Err(error.context("sensor acquisition completed after its deadline"))
                    }
                };
            }
        }
        let slot = &mut self.slots[done.index];
        let timed_out =
            matches!(slot.acquisition, Acquisition::TimedOut { epoch, .. } if epoch == done.epoch);
        slot.acquisition = Acquisition::Idle(done.device);
        if done.epoch != self.epoch {
            slot.acquisition.device_mut().unwrap().invalidate();
            slot.next_due = Duration::ZERO;
            return Ok(false);
        }
        slot.sequence = slot.sequence.wrapping_add(1);
        slot.next_due = done
            .captured_at
            .saturating_add(slot.plan.interval.unwrap_or(interval));
        let device = slot.acquisition.device().unwrap();
        let generation = device.generation();
        let sleeping = device.config.atasmart.is_some();
        let mut fatal = None;
        let states = match done.result {
            Ok(values) => {
                if slot.errors > 0 {
                    tracing::info!(
                        sensor = done.index,
                        backend = backend_name(&slot.plan.source), device = %device_label(&slot.plan.source),
                        previous_errors = slot.errors,
                        "sensor acquisition recovered"
                    );
                }
                slot.errors = 0;
                values
                    .into_iter()
                    .map(|v| {
                        v.map_or(
                            if sleeping {
                                SampleState::Sleeping
                            } else {
                                SampleState::Unavailable
                            },
                            SampleState::Value,
                        )
                    })
                    .collect::<Vec<_>>()
            }
            Err(error) => {
                if !timed_out {
                    slot.errors = slot.errors.saturating_add(1);
                }
                let missing = error_is_missing(&error);
                if !late_completion {
                    tracing::warn!(sensor = done.index, backend = backend_name(&slot.plan.source), device = %device_label(&slot.plan.source), errors = slot.errors, max_errors = slot.plan.max_errors, error = %format_args!("{error:#}"), "sensor unavailable");
                }
                if !slot.plan.optional && slot.errors > slot.plan.max_errors {
                    fatal =
                        Some(error.context(format!("sensor {} exceeded error budget", done.index)));
                }
                vec![
                    if missing {
                        SampleState::Missing
                    } else {
                        SampleState::ReadError
                    };
                    slot.plan.channels.len()
                ]
            }
        };
        for (i, state) in slot.plan.channels.clone().zip(states) {
            self.samples[i] = Sample {
                state,
                captured_at: done.captured_at,
                valid_until: if matches!(state, SampleState::Value(_)) {
                    done.captured_at
                        .saturating_add(slot.plan.max_age.unwrap_or(interval * 2))
                } else {
                    done.captured_at
                },
                sequence: slot.sequence,
                generation,
            };
        }
        if let Some(error) = fatal {
            return Err(error);
        }
        Ok(true)
    }
    pub(crate) fn drain(&mut self, interval: Duration, now: Duration) -> Result<bool> {
        let mut changed = false;
        let mut fatal = None;
        while let Ok(done) = self.results.try_recv() {
            crate::metrics::record(
                crate::metrics::Operation::ResultDelay,
                now.saturating_sub(done.captured_at),
            );
            match self.accept(done, interval, true) {
                Ok(value) => changed |= value,
                Err(error) => fatal = Some(error),
            }
        }
        self.update_uncertain();
        if let Some(error) = fatal {
            return Err(error);
        }
        Ok(changed)
    }
    pub(crate) fn initial_timeout(&self, interval: Duration) -> Duration {
        self.slots
            .iter()
            .map(|s| s.plan.read_timeout.unwrap_or(interval * 2))
            .max()
            .unwrap_or(interval * 2)
    }
    /// Diagnostic state includes failed in-flight attempts, whose previous sample
    /// may still be retained for policy bookkeeping.
    pub(crate) fn diagnostic_states(&self, now: Duration) -> Vec<&'static str> {
        use crate::policy::sample::Availability;
        let mut states = vec!["unavailable"; self.samples.len()];
        for slot in &self.slots {
            for i in slot.plan.channels.clone() {
                states[i] = if slot.errors > 0 {
                    if matches!(self.samples[i].availability(now), Availability::Missing) {
                        "missing"
                    } else {
                        "read-error"
                    }
                } else if slot.sequence == 0 {
                    "pending"
                } else {
                    match self.samples[i].availability(now) {
                        Availability::Fresh => "fresh",
                        Availability::Expired => "expired",
                        Availability::Sleeping => "sleeping",
                        Availability::Missing => "missing",
                        Availability::ReadError => "read-error",
                        Availability::Unavailable => "unavailable",
                    }
                };
            }
        }
        states
    }
    pub(crate) fn initialized(&self) -> bool {
        self.slots.iter().all(|s| s.sequence > 0)
    }
    pub(crate) fn async_delay(
        &mut self,
        now: Duration,
        fallback: Duration,
        interval: Duration,
    ) -> Duration {
        for slot in &mut self.slots {
            if slot.plan.interval.is_none()
                && slot.sequence > 0
                && slot.acquisition.device().is_some()
            {
                slot.next_due = self.samples[slot.plan.channels.start]
                    .captured_at
                    .saturating_add(fallback);
            }
        }
        let due = self
            .slots
            .iter()
            .filter(|s| s.acquisition.device().is_some() && !s.waiting_capacity)
            .map(|s| s.next_due.saturating_sub(now))
            .min()
            .unwrap_or(interval);
        let expiry = self.expiry_delay(now, interval);
        // Capacity waits are resumed by Waker, not by a short retry timer.
        let timeout = self
            .slots
            .iter()
            .filter_map(|s| match s.acquisition {
                Acquisition::InFlight { started, epoch } if epoch == self.epoch => Some(
                    started
                        .saturating_add(s.plan.read_timeout.unwrap_or(interval * 2))
                        .saturating_sub(now),
                ),
                _ => None,
            })
            .min()
            .unwrap_or(interval);
        due.min(expiry)
            .min(timeout)
            .min(fallback)
            .max(Duration::from_millis(10))
    }
    fn expiry_delay(&self, now: Duration, fallback: Duration) -> Duration {
        self.samples
            .iter()
            .filter(|s| matches!(s.state, SampleState::Value(_)) && s.valid_until > now)
            .map(|s| s.valid_until.saturating_sub(now))
            .min()
            .unwrap_or(fallback)
    }
    pub fn next_delay(&self, now: Duration, fallback: Duration) -> Duration {
        let due = self
            .slots
            .iter()
            .filter(|s| s.plan.interval.is_some())
            .map(|s| s.next_due.saturating_sub(now))
            .min()
            .unwrap_or(fallback);
        let expiry = self.expiry_delay(now, fallback);
        fallback.min(due).min(expiry).max(Duration::from_millis(1))
    }
    pub fn check_hardware(&mut self) -> Result<()> {
        let mut available = 0;
        let lm = self
            .slots
            .iter()
            .any(|s| s.acquisition.device().unwrap().is_lm())
            .then(crate::hardware::backends::LmSession::open);
        for slot in &mut self.slots {
            let result = read_device(slot.acquisition.device_mut().unwrap(), lm.as_ref());
            match result {
                Ok(samples) => available += samples.iter().flatten().count(),
                Err(_) if slot.plan.optional => (),
                Err(error) => return Err(error).context("checking sensor hardware"),
            }
        }
        ensure!(available > 0, "no sensor currently provides a temperature");
        Ok(())
    }
    pub fn reset(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        for slot in &mut self.slots {
            if let Some(device) = slot.acquisition.device_mut() {
                device.invalidate();
            }
            slot.next_due = Duration::ZERO;
            slot.waiting_capacity = false;
            slot.sequence = 0;
            slot.errors = 0;
        }
        self.uncertain = false;
        for sample in &mut self.samples {
            *sample = Sample::default();
        }
    }
}
fn read_device(
    device: &mut ConfiguredSensor,
    lm: Option<&Result<crate::hardware::backends::LmSession>>,
) -> Result<Vec<Option<f64>>> {
    if device.is_lm() {
        match lm {
            Some(Ok(session)) => device.read_in_session(Some(session)),
            Some(Err(error)) => Err(anyhow::anyhow!(
                "libsensors initialization failed: {error:#}"
            )),
            None => unreachable!("libsensors batch must have a session"),
        }
    } else {
        device.read()
    }
}
fn error_is_missing(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    fn test_hub() -> SensorHub {
        let config = Config::parse("sensors: [{hwmon: /unused/temp, max_errors: 1}]\nfans: [{hwmon: /unused/pwm}]\nlevels: [[180, 0, 120]]").unwrap();
        let plan = config.compile(&[1]).unwrap();
        let device = ConfiguredSensor::new(config.sensors[0].clone(), false).unwrap();
        SensorHub::new(vec![device], plan.sensors, 1)
    }
    #[test]
    fn queued_timely_completion_cannot_exhaust_zero_error_budget() {
        let interval = Duration::from_secs(1);
        for timer_first in [false, true] {
            let mut hub = test_hub();
            hub.slots[0].plan.max_errors = 0;
            let device = hub.slots[0].acquisition.take(Duration::ZERO, 0);
            // The event loop initially sees no results. The worker publishes
            // before the deadline, between that drain and the timeout check.
            assert!(!hub.drain(interval, Duration::from_millis(1900)).unwrap());
            hub.sender
                .send(Completion {
                    index: 0,
                    epoch: 0,
                    device,
                    captured_at: Duration::from_millis(1999),
                    result: Ok(vec![Some(40.0)]),
                })
                .unwrap();
            let now = Duration::from_millis(2100);
            if timer_first {
                hub.check_timeouts(now, interval).unwrap();
            }
            hub.drain(interval, now).unwrap();
            hub.check_timeouts(now, interval).unwrap();
            assert_eq!(hub.slots[0].errors, 0);
            assert_eq!(hub.samples[0].state, SampleState::Value(40.0));
            assert_eq!(hub.samples[0].captured_at, Duration::from_millis(1999));
            assert!(!hub.uncertain);
        }
    }
    #[test]
    fn queued_late_failure_keeps_original_cause_and_exhausts_budget_once() {
        let mut hub = test_hub();
        hub.slots[0].plan.max_errors = 0;
        let device = hub.slots[0].acquisition.take(Duration::ZERO, 0);
        hub.sender
            .send(Completion {
                index: 0,
                epoch: 0,
                device,
                captured_at: Duration::from_secs(2),
                result: Err(anyhow::anyhow!("NVML test failure")),
            })
            .unwrap();
        let error = hub
            .check_timeouts(Duration::from_secs(3), Duration::from_secs(1))
            .unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("completed after its deadline"),
            "{message}"
        );
        assert!(message.contains("NVML test failure"), "{message}");
        assert_eq!(hub.slots[0].errors, 1);
        assert_eq!(hub.samples[0].state, SampleState::ReadError);
        assert!(hub.uncertain);
    }
    #[test]
    fn optional_timeout_uses_its_own_deadline_and_counts_once() {
        let mut hub = test_hub();
        hub.slots[0].plan.optional = true;
        hub.slots[0].plan.max_errors = 0;
        hub.slots[0].plan.read_timeout = Some(Duration::from_secs(5));
        let device = hub.slots[0].acquisition.take(Duration::ZERO, 0);
        let interval = Duration::from_secs(1);
        assert_eq!(hub.diagnostic_states(Duration::ZERO), ["pending"]);
        hub.check_timeouts(Duration::from_secs(5) - Duration::from_nanos(1), interval)
            .unwrap();
        assert_eq!(hub.slots[0].errors, 0);
        for at in [5, 6, 100] {
            hub.check_timeouts(Duration::from_secs(at), interval)
                .unwrap();
        }
        assert_eq!(hub.slots[0].errors, 1);
        assert!(!hub.uncertain);
        assert_eq!(
            hub.diagnostic_states(Duration::from_secs(100)),
            ["read-error"]
        );
        hub.sender
            .send(Completion {
                index: 0,
                epoch: 0,
                device,
                captured_at: Duration::from_secs(100),
                result: Ok(vec![Some(40.0)]),
            })
            .unwrap();
        hub.drain(interval, Duration::from_secs(100)).unwrap();
        assert_eq!(hub.slots[0].errors, 1);
        assert!(!hub.uncertain);
        assert_eq!(hub.samples[0].state, SampleState::ReadError);
    }
    #[test]
    fn late_success_is_failure_regardless_of_timeout_drain_order() {
        let interval = Duration::from_secs(1);
        for timer_first in [false, true] {
            for captured_at in [Duration::from_secs(2), Duration::from_secs(3)] {
                let mut hub = test_hub();
                let device = hub.slots[0].acquisition.take(Duration::ZERO, 0);
                if timer_first {
                    hub.check_timeouts(captured_at, interval).unwrap();
                }
                hub.sender
                    .send(Completion {
                        index: 0,
                        epoch: 0,
                        device,
                        captured_at,
                        result: Ok(vec![Some(40.0)]),
                    })
                    .unwrap();
                assert!(hub.drain(interval, captured_at).unwrap());
                hub.check_timeouts(captured_at, interval).unwrap();
                assert_eq!(hub.slots[0].errors, 1);
                assert!(hub.uncertain);
                assert!(matches!(hub.samples[0].state, SampleState::ReadError));
                // Only a fresh acquisition can clear the failure.
                let device = hub.slots[0].acquisition.take(captured_at, 0);
                hub.sender
                    .send(Completion {
                        index: 0,
                        epoch: 0,
                        device,
                        captured_at: captured_at + interval,
                        result: Ok(vec![Some(41.0)]),
                    })
                    .unwrap();
                hub.drain(interval, captured_at + interval).unwrap();
                assert_eq!(hub.slots[0].errors, 0);
                assert!(!hub.uncertain);
            }
        }
    }
    #[test]
    fn timely_completion_delivered_later_is_valid_but_old_epoch_is_discarded() {
        let mut hub = test_hub();
        let device = hub.slots[0].acquisition.take(Duration::ZERO, 0);
        hub.sender
            .send(Completion {
                index: 0,
                epoch: 0,
                device,
                captured_at: Duration::from_secs(1),
                result: Ok(vec![Some(40.0)]),
            })
            .unwrap();
        hub.drain(Duration::from_secs(1), Duration::from_secs(5))
            .unwrap();
        assert_eq!(hub.slots[0].errors, 0);
        assert_eq!(hub.samples[0].captured_at, Duration::from_secs(1));
        assert_eq!(hub.samples[0].valid_until, Duration::from_secs(3));
        let device = hub.slots[0].acquisition.take(Duration::from_secs(5), 0);
        hub.reset();
        hub.sender
            .send(Completion {
                index: 0,
                epoch: 0,
                device,
                captured_at: Duration::from_secs(10),
                result: Ok(vec![Some(90.0)]),
            })
            .unwrap();
        assert!(
            !hub.drain(Duration::from_secs(1), Duration::from_secs(10))
                .unwrap()
        );
        assert_eq!(hub.slots[0].errors, 0);
        assert_eq!(hub.samples[0].sequence, 0);
    }
    #[test]
    fn saturated_capacity_does_not_create_a_short_retry_deadline() {
        let mut hub = test_hub();
        hub.slots[0].waiting_capacity = true;
        let interval = Duration::from_secs(5);
        assert_eq!(
            hub.async_delay(Duration::ZERO, interval, interval),
            interval
        );
        hub.samples[0] = Sample {
            state: SampleState::Value(40.0),
            valid_until: Duration::from_millis(300),
            ..Sample::default()
        };
        assert_eq!(
            hub.async_delay(Duration::ZERO, interval, interval),
            Duration::from_millis(300)
        );
    }
    #[test]
    fn timeout_and_late_failure_consume_one_error_budget_entry() {
        let mut hub = test_hub();
        let device = hub.slots[0].acquisition.take(Duration::ZERO, 0);
        hub.check_timeouts(Duration::from_secs(3), Duration::from_secs(1))
            .unwrap();
        hub.sender
            .send(Completion {
                index: 0,
                epoch: 0,
                device,
                captured_at: Duration::from_secs(4),
                result: Err(anyhow::anyhow!("late failure")),
            })
            .unwrap();
        assert!(
            hub.drain(Duration::from_secs(1), Duration::from_secs(4))
                .unwrap()
        );
        assert_eq!(hub.slots[0].errors, 1);
        assert!(hub.uncertain);
        hub.slots[0].acquisition.take(Duration::from_secs(5), 0);
        assert!(
            hub.check_timeouts(Duration::from_secs(8), Duration::from_secs(1))
                .is_err()
        );
    }
}
