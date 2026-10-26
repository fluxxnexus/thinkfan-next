//! Hardware and policy coordination, including the synchronous library API.
use super::{
    Options,
    adaptive::AdaptiveSampling,
    diagnostics::{self, Diagnostics},
};
use crate::{
    config::Config,
    hardware::{ConfiguredSensor, FanManager, SensorDevice, SensorHub},
    policy::{DecisionReason, PolicyEngine, control::Speed},
};
use anyhow::Result;
use std::time::{Duration, Instant};
pub struct Controller {
    pub(super) sensors: SensorHub,
    pub(super) fans: FanManager,
    pub(super) policy: PolicyEngine,
    pub(super) clock: Instant,
    pub(super) options: Options,
    pub temperatures: Vec<Option<f64>>,
    pub effective_temperatures: Vec<Option<f64>>,
    pub speeds: Vec<Speed>,
    pub next_interval: Duration,
    pub(super) adaptive: AdaptiveSampling,
    diagnostics: Diagnostics,
}
impl Controller {
    pub fn prepare(config: Config, options: Options) -> Result<Self> {
        options.validate()?;
        config.validate()?;
        let devices = config
            .sensors
            .iter()
            .map(|s| ConfiguredSensor::new(s.clone(), options.no_disk_wakeup))
            .collect::<Result<Vec<_>>>()?;
        let widths = devices.iter().map(SensorDevice::width).collect::<Vec<_>>();
        let plan = config.compile(&widths)?;
        let policy = PolicyEngine::new(&plan, options.bias);
        let diagnostics = Diagnostics::new(&plan);
        let fans = FanManager::prepare(plan.fans, options.watchdog, options.dry_run)?;
        let sensors = SensorHub::new(devices, plan.sensors, plan.channel_count);
        Ok(Self {
            sensors,
            diagnostics,
            fans,
            policy,
            clock: Instant::now(),
            temperatures: vec![None; plan.channel_count],
            effective_temperatures: vec![None; plan.channel_count],
            speeds: Vec::new(),
            next_interval: options.interval,
            adaptive: AdaptiveSampling::new(options.interval, plan.channel_count),
            options,
        })
    }
    pub fn check_hardware(&mut self) -> Result<()> {
        self.sensors.check_hardware()?;
        self.fans.check_hardware()
    }
    pub fn tick(&mut self) -> Result<()> {
        self.tick_with_time(None)
    }
    /// Hardware integration test hook. Pure replay is PolicyEngine::evaluate.
    pub fn tick_at(&mut self, now: Duration) -> Result<()> {
        self.tick_with_time(Some(now))
    }
    fn tick_with_time(&mut self, now: Option<Duration>) -> Result<()> {
        let _span = tracing::debug_span!("control_cycle").entered();
        let _measure = crate::metrics::Measure::start(crate::metrics::Operation::Cycle);
        let result = self.tick_inner(now);
        if result.is_err() {
            self.emergency_all();
        }
        result
    }
    fn tick_inner(&mut self, at: Option<Duration>) -> Result<()> {
        if let Err(error) = self.sensors.collect(at, self.clock, self.options.interval) {
            self.policy.force_emergency(DecisionReason::SensorFault);
            return Err(error);
        }
        self.apply_samples(at, None, true)
    }
    pub(super) fn apply_samples(
        &mut self,
        at: Option<Duration>,
        async_fan_fault: Option<bool>,
        maintenance_due: bool,
    ) -> Result<()> {
        let _measure = async_fan_fault
            .is_some()
            .then(|| crate::metrics::Measure::start(crate::metrics::Operation::Cycle));
        let fan_fault = match async_fan_fault.map_or_else(|| self.fans.refresh(), Ok) {
            Ok(fault) => fault,
            Err(error) => {
                self.policy.force_emergency(DecisionReason::FanFault);
                return Err(error);
            }
        };
        let now = at.unwrap_or_else(|| self.clock.elapsed());
        self.policy
            .evaluate(&self.sensors.samples, now, self.sensors.uncertain)?;
        if fan_fault {
            self.policy.force_emergency(DecisionReason::FanFault);
        }
        self.adaptive.update(
            &self.sensors.samples,
            now,
            self.policy.decision.rising,
            self.policy.fresh_rising(),
            self.policy.decision.reason == DecisionReason::Curve,
        );
        let report = self.fans.apply_scheduled(
            &self.policy.decision.commands,
            self.emergency_active(),
            Instant::now(),
            if async_fan_fault.is_some() && !maintenance_due {
                self.options.interval
            } else {
                Duration::ZERO
            },
        );
        if report.protective {
            self.adaptive.interrupt();
        }
        if report.protective && !self.emergency_active() {
            self.policy.force_emergency(DecisionReason::FanFault);
        }
        self.sync_snapshot();
        self.next_interval = self.sensors.next_delay(
            at.unwrap_or_else(|| self.clock.elapsed()),
            self.adaptive.interval(),
        );
        if let Some(error) = report.fatal {
            return Err(error);
        }
        if (self.options.verbose || self.options.dry_run) && tracing::enabled!(tracing::Level::INFO)
        {
            let states = self.sensors.diagnostic_states(now);
            let state = self.diagnostics.state(
                self.policy.decision.reason,
                &states,
                self.policy.cooling_pending(),
            );
            let sequences = self
                .sensors
                .samples
                .iter()
                .map(|sample| sample.sequence)
                .collect::<Vec<_>>();
            let summary_due = self.diagnostics.should_log(
                now,
                self.options.interval,
                state,
                &self.policy.decision.commands,
                &sequences,
                &states,
            );
            if self.options.verbose || summary_due {
                self.log_control("fan control sample", now, true);
            } else if tracing::enabled!(tracing::Level::DEBUG) {
                self.log_control("fan control sample", now, false);
            }
        } else if tracing::enabled!(tracing::Level::DEBUG) {
            self.log_control("fan control sample", now, false);
        }
        Ok(())
    }
    fn sync_snapshot(&mut self) {
        self.temperatures.clone_from(&self.policy.temperatures);
        self.effective_temperatures
            .clone_from(&self.policy.effective_temperatures);
        self.speeds.clear();
        self.speeds
            .extend(self.policy.decision.commands.iter().map(|c| c.speed()));
    }
    pub(super) fn log_snapshot(&self, message: &str) {
        self.log_control(message, self.clock.elapsed(), true);
    }
    fn log_control(&self, message: &str, now: Duration, info: bool) {
        let states = self.sensors.diagnostic_states(now);
        let state = self.diagnostics.state(
            self.policy.decision.reason,
            &states,
            self.policy.cooling_pending(),
        );
        let temperatures = self.diagnostics.temperatures(&self.temperatures, &states);
        let prediction = self
            .diagnostics
            .prediction(&self.temperatures, &self.effective_temperatures);
        let predicted = (!prediction.is_empty()).then(|| tracing::field::display(&prediction));
        let targets = diagnostics::targets(&self.policy.decision.commands);
        let mode = if self.options.dry_run {
            "dry-run (no fan writes)"
        } else {
            "control"
        };
        macro_rules! emit {
            ($level:expr) => {
                tracing::event!($level, %mode, %temperatures, predicted, target = %targets, %state, "{message}")
            };
        }
        if info {
            emit!(tracing::Level::INFO);
        } else {
            emit!(tracing::Level::DEBUG);
        }
    }
    pub(super) fn emergency_active(&self) -> bool {
        self.policy.decision.reason != DecisionReason::Curve
    }
    pub fn emergency_all(&mut self) {
        self.adaptive.interrupt();
        let reason = if self.policy.decision.reason == DecisionReason::SensorFault {
            DecisionReason::SensorFault
        } else {
            DecisionReason::FanFault
        };
        self.policy.force_emergency(reason);
        self.fans.emergency();
        self.sync_snapshot();
    }
    pub fn restore(&mut self) -> Result<()> {
        self.fans.restore()
    }
    pub fn reset(&mut self) {
        self.policy.reset();
        self.sensors.reset();
        self.adaptive.reset();
        self.diagnostics.reset();
        self.next_interval = self.options.interval;
    }
    pub fn depulse_start(&mut self) -> Result<()> {
        if self.emergency_active() {
            return Ok(());
        }
        self.fans.depulse_start()
    }
    pub fn depulse_end(&mut self) -> Result<()> {
        self.fans.depulse_end(&self.policy.decision.commands)
    }
    pub fn reload(&mut self, config: Config) -> Result<()> {
        let mut next = Self::prepare(config, self.options.clone())?;
        next.check_hardware()?;
        self.commit_reload(next)
    }
    pub(super) fn commit_reload(&mut self, mut next: Self) -> Result<()> {
        if let Err(error) = self.fans.commit_reload(&mut next.fans) {
            self.policy.force_emergency(DecisionReason::FanFault);
            self.sync_snapshot();
            return Err(error);
        }
        *self = next;
        Ok(())
    }
}
