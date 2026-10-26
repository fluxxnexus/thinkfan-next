//! Validated, immutable layout. Compilation performs no hardware writes or reads.
use super::{Bound, Config, FanConfig, LevelConfig, SensorConfig, Speeds};
use crate::policy::control::{Curve, FanCommand, Level, Prediction, Speed};
use anyhow::{Context, Result, bail, ensure};
use std::{
    ops::Range,
    path::{Path, PathBuf},
    time::Duration,
};
#[derive(Clone, Debug)]
pub struct HwmonSelector {
    pub path: PathBuf,
    pub name: Option<String>,
    pub model: Option<String>,
    pub indices: Option<Vec<usize>>,
}
#[derive(Clone, Debug)]
pub enum SensorSource {
    Hwmon(HwmonSelector),
    ThinkPad {
        path: PathBuf,
        indices: Option<Vec<usize>>,
    },
    LmSensors {
        chip: String,
        ids: Vec<String>,
    },
    Nvml {
        bus: String,
    },
    AtaSmart {
        path: PathBuf,
    },
}
#[derive(Clone, Debug)]
pub enum FanSelector {
    Hwmon(HwmonSelector),
    ThinkPad(PathBuf),
}
impl FanSelector {
    pub fn thinkpad(&self) -> bool {
        matches!(self, Self::ThinkPad(_))
    }
}
#[derive(Clone, Debug)]
pub struct SensorPlan {
    pub interval: Option<Duration>,
    pub max_age: Option<Duration>,
    pub read_timeout: Option<Duration>,
    pub source: SensorSource,
    pub channels: Range<usize>,
    pub correction: Vec<f64>,
    pub optional: bool,
    pub max_errors: u32,
}
#[derive(Clone, Debug)]
pub struct FanPlan {
    pub channels: Vec<usize>,
    pub selector: FanSelector,
    pub optional: bool,
    pub max_errors: u32,
    pub curve: Curve,
    pub commands: Vec<FanCommand>,
}
impl FanPlan {
    /// Curves are validated in ascending order; the final entry is the fault target.
    pub fn fault_command(&self) -> FanCommand {
        *self.commands.last().expect("validated nonempty fan curve")
    }
}
#[derive(Clone, Debug)]
pub struct RuntimePlan {
    pub sensors: Vec<SensorPlan>,
    pub fans: Vec<FanPlan>,
    pub prediction: Prediction,
    pub channel_count: usize,
}
fn selector(
    path: &Path,
    name: &Option<String>,
    model: &Option<String>,
    indices: &Option<Vec<usize>>,
) -> HwmonSelector {
    HwmonSelector {
        path: path.to_owned(),
        name: name.clone(),
        model: model.clone(),
        indices: indices.clone(),
    }
}
impl SensorConfig {
    pub fn source(&self) -> Result<SensorSource> {
        self.validate()?;
        Ok(if let Some(path) = &self.hwmon {
            SensorSource::Hwmon(selector(path, &self.name, &self.model, &self.indices))
        } else if let Some(path) = &self.tpacpi {
            SensorSource::ThinkPad {
                path: path.clone(),
                indices: self.indices.clone(),
            }
        } else if let Some(chip) = &self.chip {
            SensorSource::LmSensors {
                chip: chip.clone(),
                ids: self.ids.clone().context("chip requires ids")?,
            }
        } else if let Some(bus) = &self.nvml {
            SensorSource::Nvml { bus: bus.clone() }
        } else {
            SensorSource::AtaSmart {
                path: self.atasmart.clone().context("missing backend")?,
            }
        })
    }
}
impl FanConfig {
    pub fn selector(&self) -> Result<FanSelector> {
        self.validate()?;
        Ok(match &self.hwmon {
            Some(path) => {
                FanSelector::Hwmon(selector(path, &self.name, &self.model, &self.indices))
            }
            None => FanSelector::ThinkPad(self.tpacpi.clone().context("missing fan backend")?),
        })
    }
}
impl Config {
    /// Widths come from configuration or read-only binding of an auto-width device.
    pub fn compile(&self, widths: &[usize]) -> Result<RuntimePlan> {
        self.validate()?;
        ensure!(
            widths.len() == self.sensors.len(),
            "sensor width layout mismatch"
        );
        let mut cursor: usize = 0;
        let mut sensors = Vec::new();
        for (config, &width) in self.sensors.iter().zip(widths) {
            config.validate_width(width)?;
            ensure!(
                config.width().is_none_or(|n| n == width),
                "sensor width disagrees with selectors"
            );
            let end = cursor
                .checked_add(width)
                .context("channel layout overflow")?;
            sensors.push(SensorPlan {
                interval: config.interval_seconds.map(Duration::from_secs_f64),
                max_age: config
                    .max_age_seconds
                    .or(config.interval_seconds.map(|s| s * 2.0))
                    .map(Duration::from_secs_f64),
                read_timeout: config.read_timeout_seconds.map(Duration::from_secs_f64),
                source: config.source()?,
                channels: cursor..end,
                correction: if config.correction.is_empty() {
                    vec![0.0; width]
                } else {
                    config.correction.clone()
                },
                optional: config.optional,
                max_errors: config.max_errors,
            });
            cursor = end;
        }
        let fans = self
            .fans
            .iter()
            .zip(self.curves(Some(cursor))?)
            .map(|(config, curve)| {
                let selector = config.selector()?;
                let commands = curve
                    .levels
                    .iter()
                    .map(|level| FanCommand::compile(&level.speed, selector.thinkpad()))
                    .collect::<Result<_>>()?;
                Ok(FanPlan {
                    channels: (0..cursor).filter(|i| curve.uses_channel(*i)).collect(),
                    selector,
                    optional: config.optional,
                    max_errors: config.max_errors,
                    curve,
                    commands,
                })
            })
            .collect::<Result<_>>()?;
        Ok(RuntimePlan {
            sensors,
            fans,
            prediction: self.control.prediction.clone(),
            channel_count: cursor,
        })
    }

    pub fn curves(&self, sensor_count: Option<usize>) -> Result<Vec<Curve>> {
        self.fans
            .iter()
            .enumerate()
            .map(|(fan_index, fan)| {
                let (levels, index, count) = match &self.levels {
                    Some(l) => (l, fan_index, self.fans.len()),
                    None => (fan.levels.as_ref().context("missing fan levels")?, 0, 1),
                };
                compile_curve(levels, index, count, sensor_count, fan.tpacpi.is_some())
                    .map(|curve| {
                        curve.with_timing(
                            Duration::from_secs_f64(
                                fan.down_delay_seconds
                                    .unwrap_or(self.control.down_delay_seconds),
                            ),
                            Duration::from_secs_f64(self.control.prediction.max_sample_gap_seconds),
                        )
                    })
                    .with_context(|| format!("fans[{fan_index}] levels"))
            })
            .collect()
    }
}

impl Speeds {
    fn values(&self) -> &[Speed] {
        match self {
            Self::One(s) => std::slice::from_ref(s),
            Self::Many(s) => s,
        }
    }
}
impl Bound {
    fn value(&self) -> Result<Option<f64>> {
        match self {
            Self::Temperature(t) => {
                ensure!(t.is_finite(), "temperature limits must be finite");
                Ok(Some(*t))
            }
            Self::Ignored(s) if s == "_" => Ok(None),
            _ => bail!("only '_' may be used to ignore a temperature limit"),
        }
    }
}
fn compile_curve(
    input: &[LevelConfig],
    fan: usize,
    fan_count: usize,
    sensor_count: Option<usize>,
    tp: bool,
) -> Result<Curve> {
    ensure!(!input.is_empty(), "levels must not be empty");
    let simple = matches!(input[0], LevelConfig::Simple(_));
    let mut width = if simple { Some(1) } else { sensor_count };
    let mut levels: Vec<Level> = Vec::new();
    for (i, raw) in input.iter().enumerate() {
        let (speeds, lower, upper) = match raw {
            LevelConfig::Simple((s, l, u)) => {
                ensure!(simple, "cannot mix simple and detailed levels");
                ensure!(l.is_finite() && u.is_finite(), "limits must be finite");
                (s, vec![Some(*l)], vec![Some(*u)])
            }
            LevelConfig::Detailed(d) => {
                ensure!(!simple, "cannot mix simple and detailed levels");
                ensure!(
                    i == 0 || d.lower_limit.is_some(),
                    "only the first lower_limit may be omitted"
                );
                ensure!(
                    i + 1 == input.len() || d.upper_limit.is_some(),
                    "only the last upper_limit may be omitted"
                );
                let inferred = d
                    .lower_limit
                    .as_ref()
                    .or(d.upper_limit.as_ref())
                    .map(Vec::len);
                let n = width.or(inferred).unwrap_or(1);
                ensure!(n > 0, "temperature limit arrays cannot be empty");
                width = Some(n);
                let bounds = |v: &Option<Vec<Bound>>| -> Result<Vec<Option<f64>>> {
                    match v {
                        None => Ok(vec![None; n]),
                        Some(v) => {
                            ensure!(v.len() == n, "temperature limit length must be {n}");
                            v.iter().map(Bound::value).collect()
                        }
                    }
                };
                (&d.speed, bounds(&d.lower_limit)?, bounds(&d.upper_limit)?)
            }
        };
        let speeds = speeds.values();
        ensure!(
            speeds.len() == fan_count,
            "expected {fan_count} fan speeds, got {}",
            speeds.len()
        );
        let speed = speeds[fan].normalized(tp)?;
        for (l, u) in lower.iter().zip(&upper) {
            if let (Some(l), Some(u)) = (l, u) {
                ensure!(l < u, "lower limits must be below upper limits");
            }
        }
        if let Some(previous) = levels.last() {
            for (old, new) in previous.upper.iter().zip(&lower) {
                if let (Some(u), Some(l)) = (old, new) {
                    ensure!(l < u, "adjacent levels need overlapping hysteresis");
                }
            }
            for (old, new) in previous
                .upper
                .iter()
                .zip(&upper)
                .chain(previous.lower.iter().zip(&lower))
            {
                if let (Some(a), Some(b)) = (old, new) {
                    ensure!(b >= a, "temperature thresholds must not decrease");
                }
            }
            if let (Speed::Numeric(a), Speed::Numeric(b)) = (&previous.speed, &speed) {
                ensure!(b >= a, "numeric fan speeds must not decrease");
            }
        }
        if i + 1 < input.len() {
            ensure!(
                upper.iter().any(Option::is_some),
                "non-final level must have an active upper limit"
            );
        }
        levels.push(Level {
            speed,
            lower,
            upper,
        });
    }
    if !tp {
        ensure!(
            matches!(levels.last().unwrap().speed, Speed::Numeric(128..=255)),
            "last PWM level must be at least 128"
        );
    }
    Ok(Curve::new(levels, simple))
}
