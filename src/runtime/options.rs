//! Validated runtime settings.
use anyhow::{Result, ensure};
use std::time::Duration;
#[derive(Clone, Debug)]
pub struct Options {
    pub interval: Duration,
    pub bias: f64,
    pub watchdog: u32,
    pub depulse: Duration,
    pub no_disk_wakeup: bool,
    pub dry_run: bool,
    pub verbose: bool,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(5),
            bias: 0.0,
            watchdog: 120,
            depulse: Duration::ZERO,
            no_disk_wakeup: false,
            dry_run: false,
            verbose: false,
        }
    }
}
impl Options {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (Duration::from_millis(100)..=Duration::from_secs(60)).contains(&self.interval),
            "interval must be 0.1..60 seconds"
        );
        ensure!(
            self.bias.is_finite() && (-10.0..=30.0).contains(&self.bias),
            "bias must be -10..30"
        );
        ensure!(
            (1..=120).contains(&self.watchdog),
            "watchdog must be 1..120 seconds"
        );
        ensure!(
            self.interval + self.depulse + Duration::from_secs(1)
                < Duration::from_secs(self.watchdog.into()),
            "watchdog must exceed interval + depulse + 1 second"
        );
        ensure!(
            self.depulse <= Duration::from_secs(10),
            "depulse must be at most 10 seconds"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn interval_bias_and_depulse_bounds_are_inclusive() {
        for interval in [Duration::from_millis(100), Duration::from_secs(60)] {
            for bias in [-10.0, 30.0] {
                Options {
                    interval,
                    bias,
                    depulse: Duration::from_secs(10),
                    ..Options::default()
                }
                .validate()
                .unwrap();
            }
        }
        for interval in [
            Duration::from_millis(99),
            Duration::from_secs(60) + Duration::from_nanos(1),
        ] {
            assert!(
                Options {
                    interval,
                    ..Options::default()
                }
                .validate()
                .unwrap_err()
                .to_string()
                .contains("interval")
            );
        }
        for bias in [f64::NAN, f64::INFINITY, -10.01, 30.01] {
            assert!(
                Options {
                    bias,
                    ..Options::default()
                }
                .validate()
                .unwrap_err()
                .to_string()
                .contains("bias")
            );
        }
        assert!(
            Options {
                depulse: Duration::from_secs(10) + Duration::from_nanos(1),
                ..Options::default()
            }
            .validate()
            .unwrap_err()
            .to_string()
            .contains("depulse")
        );
    }
    #[test]
    fn watchdog_must_strictly_exceed_interval_plus_depulse_and_margin() {
        let mut options = Options {
            interval: Duration::from_secs(5),
            depulse: Duration::from_secs(1),
            watchdog: 7,
            ..Options::default()
        };
        assert!(
            options
                .validate()
                .unwrap_err()
                .to_string()
                .contains("must exceed")
        );
        options.watchdog = 8;
        options.validate().unwrap();
        for watchdog in [0, 121] {
            options.watchdog = watchdog;
            assert!(
                options
                    .validate()
                    .unwrap_err()
                    .to_string()
                    .contains("1..120")
            );
        }
    }
}
