//! Pure control policy; no filesystem, clocks, signals or native libraries.
use anyhow::{Result, bail, ensure};
use serde::Deserialize;
use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum Speed {
    Numeric(u16),
    Mode(String),
}
impl Speed {
    pub fn normalized(&self, thinkpad: bool) -> Result<Self> {
        match self {
            Self::Numeric(n) => {
                ensure!(
                    *n <= if thinkpad { 7 } else { 255 },
                    "fan speed out of range"
                );
                Ok(self.clone())
            }
            Self::Mode(m) if thinkpad => {
                let s = m.strip_prefix("level ").unwrap_or(m);
                if let Ok(n) = s.parse::<u16>() {
                    return Self::Numeric(n).normalized(true);
                }
                match s {
                    "auto" | "disengaged" | "full-speed" => Ok(Self::Mode(s.into())),
                    _ => bail!("invalid ThinkPad mode: {m}"),
                }
            }
            _ => bail!("PWM fans require numeric speeds"),
        }
    }
    pub fn command(&self, thinkpad: bool) -> String {
        let value = match self {
            Self::Numeric(n) => n.to_string(),
            Self::Mode(m) => m.clone(),
        };
        if thinkpad {
            format!("level {value}")
        } else {
            value
        }
    }
}
#[derive(Clone, Debug)]
pub struct Level {
    pub speed: Speed,
    pub lower: Vec<Option<f64>>,
    pub upper: Vec<Option<f64>>,
}
#[derive(Clone, Debug)]
pub struct Curve {
    pub levels: Vec<Level>,
    simple: bool,
    current: Option<usize>,
    down_delay: Duration,
    down_since: Option<Duration>,
    last_sample: Option<Duration>,
    max_gap: Duration,
}
impl Curve {
    pub fn new(levels: Vec<Level>, simple: bool) -> Self {
        Self {
            levels,
            simple,
            current: None,
            down_delay: Duration::ZERO,
            down_since: None,
            last_sample: None,
            max_gap: Duration::from_secs(120),
        }
    }
    pub fn reset(&mut self) {
        self.current = None;
        self.down_since = None;
        self.last_sample = None;
    }
    pub fn with_timing(mut self, delay: Duration, max_gap: Duration) -> Self {
        self.down_delay = delay;
        self.max_gap = max_gap;
        self
    }
    pub fn current_index(&self) -> usize {
        self.current.unwrap_or(self.levels.len() - 1)
    }
    pub(crate) fn cooling_pending(&self) -> bool {
        self.down_since.is_some()
    }
    pub fn hold_high(&mut self) {
        self.current = Some(self.levels.len() - 1);
        self.down_since = None;
        self.last_sample = None;
    }
    /// Convenience for untimed curves (zero down delay). Use select_at
    /// for configured time-dependent policies and deterministic replay.
    pub fn select(&mut self, samples: &[Option<f64>]) -> &Speed {
        self.select_at(samples, Duration::ZERO)
    }
    pub fn uses_channel(&self, index: usize) -> bool {
        self.simple
            || self.levels.iter().any(|l| {
                l.lower.get(index).is_some_and(Option::is_some)
                    || l.upper.get(index).is_some_and(Option::is_some)
            })
    }
    pub fn interrupt_timing(&mut self) {
        self.down_since = None;
        self.last_sample = None;
    }
    pub fn select_at(&mut self, samples: &[Option<f64>], now: Duration) -> &Speed {
        let maximum = samples.iter().flatten().copied().reduce(f64::max);
        self.select_with_maximum(samples, maximum, now)
    }
    pub fn select_with_maximum(
        &mut self,
        samples: &[Option<f64>],
        maximum: Option<f64>,
        now: Duration,
    ) -> &Speed {
        self.select_observed(samples, maximum, now, now)
    }
    pub fn select_observed(
        &mut self,
        samples: &[Option<f64>],
        maximum: Option<f64>,
        now: Duration,
        evidence: Duration,
    ) -> &Speed {
        if self
            .last_sample
            .is_some_and(|last| now < last || now.saturating_sub(last) > self.max_gap)
        {
            self.down_since = None;
        }
        self.last_sample = Some(now);
        let simple_values = [maximum];
        let values = if self.simple {
            &simple_values[..]
        } else {
            samples
        };
        let up = |i: usize| {
            self.levels[i]
                .upper
                .iter()
                .zip(values)
                .any(|(limit, sample)| matches!((limit, sample), (Some(l), Some(t)) if t >= l))
        };
        let down = |i: usize| {
            let has_reading = self.levels[i]
                .lower
                .iter()
                .zip(values)
                .any(|(limit, sample)| limit.is_some() && sample.is_some());
            (self.down_delay.is_zero() || has_reading)
                && self.levels[i]
                    .lower
                    .iter()
                    .zip(values)
                    .all(|(limit, sample)| match (limit, sample) {
                        (Some(l), Some(t)) => t < l,
                        _ => true,
                    })
        };
        // Start at the highest level and descend: an overlap starts conservatively.
        let mut current = self.current.unwrap_or(self.levels.len() - 1);
        if self.current.is_none() {
            while current > 0 && down(current) {
                current -= 1;
            }
        } else if current + 1 < self.levels.len() && up(current) {
            self.down_since = None;
            while current + 1 < self.levels.len() && up(current) {
                current += 1;
            }
        } else if current > 0 && down(current) {
            if self.down_delay.is_zero() {
                while current > 0 && down(current) {
                    current -= 1;
                }
            } else {
                let since = *self.down_since.get_or_insert(now);
                if evidence.saturating_sub(since) >= self.down_delay {
                    current -= 1;
                    self.down_since = None;
                }
            }
        } else {
            self.down_since = None;
        }
        self.current = Some(current);
        &self.levels[current].speed
    }
}
/// Time constants are seconds; rise_rate is Celsius per second.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Prediction {
    pub rise_rate: f64,
    pub rate_smoothing_seconds: f64,
    pub bias_decay_seconds: f64,
    pub max_bias: f64,
    pub max_sample_gap_seconds: f64,
}
impl Default for Prediction {
    fn default() -> Self {
        Self {
            rise_rate: 0.4,
            rate_smoothing_seconds: 2.0,
            bias_decay_seconds: 25.0,
            max_bias: 30.0,
            max_sample_gap_seconds: 120.0,
        }
    }
}
impl Prediction {
    pub fn validate(&self) -> Result<()> {
        for (name, value) in [
            ("rise_rate", self.rise_rate),
            ("rate_smoothing_seconds", self.rate_smoothing_seconds),
            ("bias_decay_seconds", self.bias_decay_seconds),
            ("max_bias", self.max_bias),
            ("max_sample_gap_seconds", self.max_sample_gap_seconds),
        ] {
            ensure!(
                value.is_finite() && value > 0.0 && value <= 86400.0,
                "prediction {name} must be finite and within 0..86400, excluding zero"
            );
        }
        Ok(())
    }
}
#[derive(Debug)]
pub struct TemperatureFilter {
    previous: Vec<Option<f64>>,
    bias: Vec<f64>,
    rate: Vec<f64>,
    previous_time: Vec<Option<Duration>>,
    coefficients: Option<(f64, f64, f64)>,
    horizon: f64,
    settings: Prediction,
}
impl TemperatureFilter {
    pub fn new(count: usize, bias: f64) -> Self {
        Self::configured(count, bias, Prediction::default())
    }
    pub fn configured(count: usize, bias: f64, settings: Prediction) -> Self {
        Self {
            previous: vec![None; count],
            bias: vec![0.0; count],
            rate: vec![0.0; count],
            previous_time: vec![None; count],
            coefficients: None,
            horizon: bias / 2.0,
            settings,
        }
    }
    pub fn reset_channel(&mut self, i: usize) {
        self.previous[i] = None;
        self.previous_time[i] = None;
        self.bias[i] = 0.0;
        self.rate[i] = 0.0;
    }
    pub fn update_channel(
        &mut self,
        i: usize,
        sample: Option<f64>,
        now: Duration,
    ) -> (Option<f64>, bool) {
        let Some(t) = sample.filter(|t| t.is_finite()) else {
            self.reset_channel(i);
            return (None, false);
        };
        let dt = self.previous_time[i]
            .and_then(|last| now.checked_sub(last))
            .map(|d| d.as_secs_f64())
            .filter(|dt| *dt > 0.0 && *dt <= self.settings.max_sample_gap_seconds);
        let mut rising = false;
        if let (Some(old), Some(dt)) = (self.previous[i], dt) {
            let (alpha, decay) = match self.coefficients {
                Some((last_dt, alpha, decay)) if last_dt == dt => (alpha, decay),
                _ => {
                    let alpha = -(-dt / self.settings.rate_smoothing_seconds).exp_m1();
                    let decay = (-dt / self.settings.bias_decay_seconds).exp();
                    self.coefficients = Some((dt, alpha, decay));
                    (alpha, decay)
                }
            };
            self.rate[i] += alpha * ((t - old) / dt - self.rate[i]);
            self.bias[i] *= decay;
            if !self.rate[i].is_finite() {
                self.rate[i] = 0.0;
                self.bias[i] = 0.0;
            }
            if self.rate[i] > self.settings.rise_rate {
                rising = true;
                let predicted = (self.rate[i] * self.horizon)
                    .clamp(-self.settings.max_bias, self.settings.max_bias);
                if predicted.abs() > self.bias[i].abs() {
                    self.bias[i] = predicted;
                }
            }
        } else {
            self.rate[i] = 0.0;
            self.bias[i] = 0.0;
        }
        self.previous[i] = Some(t);
        self.previous_time[i] = Some(now);
        let effective = t + self.bias[i];
        (
            Some(if effective.is_finite() { effective } else { t }),
            rising,
        )
    }
    pub fn update_into(
        &mut self,
        samples: &[Option<f64>],
        now: Duration,
        output: &mut [Option<f64>],
    ) -> bool {
        let mut rising = false;
        for (i, (sample, out)) in samples.iter().zip(output).enumerate() {
            let (value, up) = self.update_channel(i, *sample, now);
            *out = value;
            rising |= up;
        }
        rising
    }
    pub fn update_at(
        &mut self,
        samples: &[Option<f64>],
        now: Duration,
    ) -> (Vec<Option<f64>>, bool) {
        let mut out = vec![None; samples.len()];
        let rising = self.update_into(samples, now, &mut out);
        (out, rising)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThinkPadCommand {
    Level(u8),
    Auto,
    Disengaged,
    FullSpeed,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FanCommand {
    Pwm(u8),
    ThinkPad(ThinkPadCommand),
}
impl FanCommand {
    pub fn compile(speed: &Speed, thinkpad: bool) -> Result<Self> {
        Ok(match speed.normalized(thinkpad)? {
            Speed::Numeric(n) if thinkpad => Self::ThinkPad(ThinkPadCommand::Level(n as u8)),
            Speed::Numeric(n) => Self::Pwm(n as u8),
            Speed::Mode(mode) => Self::ThinkPad(match mode.as_str() {
                "auto" => ThinkPadCommand::Auto,
                "disengaged" => ThinkPadCommand::Disengaged,
                "full-speed" => ThinkPadCommand::FullSpeed,
                _ => unreachable!("normalized mode"),
            }),
        })
    }
    pub fn speed(self) -> Speed {
        match self {
            Self::Pwm(n) | Self::ThinkPad(ThinkPadCommand::Level(n)) => Speed::Numeric(n.into()),
            Self::ThinkPad(mode) => Speed::Mode(
                match mode {
                    ThinkPadCommand::Auto => "auto",
                    ThinkPadCommand::Disengaged => "disengaged",
                    ThinkPadCommand::FullSpeed => "full-speed",
                    ThinkPadCommand::Level(_) => unreachable!(),
                }
                .into(),
            ),
        }
    }
}
impl std::fmt::Display for FanCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pwm(n) => write!(f, "{n}"),
            Self::ThinkPad(ThinkPadCommand::Level(n)) => write!(f, "level {n}"),
            Self::ThinkPad(ThinkPadCommand::Auto) => f.write_str("level auto"),
            Self::ThinkPad(ThinkPadCommand::Disengaged) => f.write_str("level disengaged"),
            Self::ThinkPad(ThinkPadCommand::FullSpeed) => f.write_str("level full-speed"),
        }
    }
}
