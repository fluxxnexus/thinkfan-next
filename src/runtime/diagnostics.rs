//! Presentation and throttling only; never changes a control decision.
use crate::{
    config::{RuntimePlan, SensorSource},
    policy::{DecisionReason, control::FanCommand},
};
use std::time::Duration;

pub(super) struct Diagnostics {
    labels: Vec<String>,
    required: Vec<bool>,
    last_at: Option<Duration>,
    last_state: &'static str,
    last_targets: Vec<FanCommand>,
    last_sequences: Vec<u64>,
    last_health: Vec<&'static str>,
}
impl Diagnostics {
    pub fn new(plan: &RuntimePlan) -> Self {
        let mut labels = Vec::new();
        let mut required = Vec::new();
        for (sensor, source) in plan.sensors.iter().enumerate() {
            for channel in 0..source.channels.len() {
                let label = match &source.source {
                    SensorSource::Hwmon(selector) => {
                        let name = selector.name.as_deref().unwrap_or("hwmon");
                        let input = selector
                            .indices
                            .as_ref()
                            .map(|v| format!("temp{}", v[channel]))
                            .unwrap_or_else(|| {
                                selector
                                    .path
                                    .file_name()
                                    .unwrap_or_default()
                                    .to_string_lossy()
                                    .into_owned()
                            });
                        format!("{name}.{input}")
                    }
                    SensorSource::Nvml { .. } => "gpu".to_owned(),
                    SensorSource::AtaSmart { path } => format!(
                        "smart.{}",
                        path.file_name().unwrap_or_default().to_string_lossy()
                    ),
                    SensorSource::LmSensors { chip, ids } => format!("{chip}.{}", ids[channel]),
                    SensorSource::ThinkPad { indices, .. } => format!(
                        "thinkpad.temp{}",
                        indices.as_ref().map_or(channel + 1, |v| v[channel])
                    ),
                };
                labels.push(format!("s{sensor}/{label}"));
                required.push(!source.optional);
            }
        }
        Self {
            labels,
            required,
            last_at: None,
            last_state: "",
            last_targets: Vec::new(),
            last_sequences: Vec::new(),
            last_health: Vec::new(),
        }
    }
    pub fn reset(&mut self) {
        self.last_at = None;
        self.last_state = "";
        self.last_targets.clear();
        self.last_sequences.clear();
        self.last_health.clear();
    }
    pub fn state(&self, reason: DecisionReason, states: &[&str], cooling: bool) -> &'static str {
        let failed = states.iter().zip(&self.required).any(|(state, required)| {
            *required && !matches!(*state, "fresh" | "pending" | "sleeping")
        });
        match reason {
            DecisionReason::FanFault => "fan-fault",
            DecisionReason::SensorFault if failed => "sensor-fault",
            _ if states.contains(&"pending") => "waiting-for-sensors",
            DecisionReason::SensorFault => "sensor-fault",
            _ if cooling => "cooling-delay",
            _ => "curve",
        }
    }
    pub fn should_log(
        &mut self,
        now: Duration,
        interval: Duration,
        state: &'static str,
        targets: &[FanCommand],
        sequences: &[u64],
        states: &[&'static str],
    ) -> bool {
        // Partial startup completions are coalesced; real health transitions
        // (including optional inputs) remain immediately visible.
        let health = || {
            states.iter().map(|state| match *state {
                "pending" | "fresh" => "available",
                other => other,
            })
        };
        let changed = self.last_state != state
            || self.last_targets != targets
            || self.last_health.iter().copied().ne(health());
        let periodic = self
            .last_at
            .is_none_or(|last| now.saturating_sub(last) >= interval)
            && self.last_sequences != sequences;
        if changed || periodic {
            self.last_at = Some(now);
            self.last_state = state;
            self.last_targets.clear();
            self.last_targets.extend_from_slice(targets);
            self.last_sequences.clear();
            self.last_sequences.extend_from_slice(sequences);
            self.last_health.clear();
            self.last_health.extend(health());
            true
        } else {
            false
        }
    }
    pub fn temperatures(&self, values: &[Option<f64>], states: &[&str]) -> String {
        self.labels
            .iter()
            .zip(values)
            .zip(states)
            .map(|((label, value), state)| match (value, *state) {
                (Some(value), "fresh") => format!("{label}={value:.2}°C"),
                _ => format!("{label}={state}"),
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
    pub fn prediction(&self, actual: &[Option<f64>], effective: &[Option<f64>]) -> String {
        self.labels
            .iter()
            .zip(actual)
            .zip(effective)
            .filter_map(|((label, actual), effective)| match (actual, effective) {
                (Some(a), Some(e)) if (a - e).abs() >= 0.005 => Some(format!("{label}={e:.2}°C")),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}
pub(super) fn targets(commands: &[FanCommand]) -> String {
    commands
        .iter()
        .enumerate()
        .map(|(i, command)| match command {
            FanCommand::Pwm(value) => format!("fan{i}:pwm={value}/255"),
            FanCommand::ThinkPad(_) => format!("fan{i}:level={}", command.speed().command(false)),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn diagnostics() -> Diagnostics {
        let config = Config::parse("sensors: [{hwmon: /unused/cpu}, {nvml: '01:00.0', optional: true}]\nfans: [{hwmon: /unused/pwm}]\nlevels: [[180, 0, 120]]").unwrap();
        Diagnostics::new(&config.compile(&[1, 1]).unwrap())
    }

    #[test]
    fn summary_coalesces_partial_results_but_preserves_changes() {
        let mut log = diagnostics();
        let interval = Duration::from_secs(2);
        let target = [FanCommand::Pwm(180)];
        assert!(log.should_log(
            Duration::ZERO,
            interval,
            "waiting-for-sensors",
            &target,
            &[0, 0],
            &["pending", "pending"]
        ));
        assert!(!log.should_log(
            Duration::from_millis(10),
            interval,
            "waiting-for-sensors",
            &target,
            &[1, 0],
            &["fresh", "pending"]
        ));
        assert!(log.should_log(
            Duration::from_millis(20),
            interval,
            "curve",
            &target,
            &[1, 1],
            &["fresh", "fresh"]
        ));
        // Maintenance without fresh samples does not repeat the snapshot.
        assert!(!log.should_log(
            Duration::from_secs(3),
            interval,
            "curve",
            &target,
            &[1, 1],
            &["fresh", "fresh"]
        ));
        assert!(log.should_log(
            Duration::from_secs(3),
            interval,
            "curve",
            &target,
            &[2, 1],
            &["fresh", "fresh"]
        ));
        assert!(!log.should_log(
            Duration::from_millis(3010),
            interval,
            "curve",
            &target,
            &[2, 2],
            &["fresh", "fresh"]
        ));
        // An optional device failure still bypasses the summary interval.
        assert!(log.should_log(
            Duration::from_millis(3020),
            interval,
            "curve",
            &target,
            &[2, 3],
            &["fresh", "read-error"]
        ));
        assert!(log.should_log(
            Duration::from_millis(3030),
            interval,
            "curve",
            &[FanCommand::Pwm(100)],
            &[2, 3],
            &["fresh", "read-error"]
        ));
        log.reset();
        assert!(log.should_log(
            Duration::ZERO,
            interval,
            "curve",
            &target,
            &[1, 1],
            &["fresh", "fresh"]
        ));
    }

    #[test]
    fn startup_fault_and_cooling_states_are_distinct() {
        let log = diagnostics();
        assert_eq!(
            log.state(DecisionReason::SensorFault, &["pending", "pending"], false),
            "waiting-for-sensors"
        );
        for failure in ["read-error", "expired", "missing", "unavailable"] {
            assert_eq!(
                log.state(DecisionReason::SensorFault, &[failure, "pending"], false),
                "sensor-fault"
            );
        }
        assert_eq!(
            log.state(DecisionReason::Curve, &["fresh", "read-error"], true),
            "cooling-delay"
        );
        assert_eq!(
            log.state(DecisionReason::FanFault, &["pending", "pending"], false),
            "fan-fault"
        );
        assert_eq!(
            log.state(DecisionReason::Curve, &["fresh", "fresh"], false),
            "curve"
        );
    }

    #[test]
    fn display_names_units_and_only_changed_predictions() {
        let log = diagnostics();
        assert_eq!(
            log.temperatures(&[Some(40.0), None], &["fresh", "pending"]),
            "s0/hwmon.cpu=40.00°C s1/gpu=pending"
        );
        assert_eq!(
            log.temperatures(&[Some(40.0), None], &["expired", "sleeping"]),
            "s0/hwmon.cpu=expired s1/gpu=sleeping"
        );
        assert!(
            log.prediction(&[Some(40.0), None], &[Some(40.0), None])
                .is_empty()
        );
        assert_eq!(
            log.prediction(&[Some(40.0), None], &[Some(42.0), None]),
            "s0/hwmon.cpu=42.00°C"
        );
        assert_eq!(
            targets(&[
                FanCommand::Pwm(180),
                FanCommand::ThinkPad(crate::policy::control::ThinkPadCommand::Level(7))
            ]),
            "fan0:pwm=180/255 fan1:level=7"
        );
    }
}
