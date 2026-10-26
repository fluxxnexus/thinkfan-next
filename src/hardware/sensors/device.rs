use crate::config::{SensorConfig, SensorSource};
use crate::hardware::backends::{self, hwmon::HwmonSensor};
use anyhow::{Context, Result, ensure};
use std::fs;

pub trait SensorDevice {
    fn read(&mut self) -> Result<Vec<Option<f64>>>;
    fn width(&self) -> usize;
}
pub struct ConfiguredSensor {
    pub config: SensorConfig,
    width: usize,
    no_disk_wakeup: bool,
    backend: SensorBackend,
}
enum SensorBackend {
    Hwmon(HwmonSensor),
    ThinkPad {
        path: std::path::PathBuf,
        indices: Option<Vec<usize>>,
    },
    LmSensors {
        chip: String,
        ids: Vec<String>,
    },
    Nvml {
        bus: String,
        device: Box<backends::NvmlSensor>,
    },
    AtaSmart {
        path: std::path::PathBuf,
        device: backends::AtaSmartSensor,
    },
}
impl ConfiguredSensor {
    pub fn new(config: SensorConfig, no_disk_wakeup: bool) -> Result<Self> {
        let source = config.source()?;
        let width = match config.width() {
            Some(n) => n,
            None => thermal(config.tpacpi.as_ref().unwrap())?.len(),
        };
        config.validate_width(width)?;
        let backend = match source {
            SensorSource::Hwmon(s) => SensorBackend::Hwmon(HwmonSensor::new(s)),
            SensorSource::ThinkPad { path, indices } => SensorBackend::ThinkPad { path, indices },
            SensorSource::LmSensors { chip, ids } => SensorBackend::LmSensors { chip, ids },
            SensorSource::Nvml { bus } => SensorBackend::Nvml {
                bus,
                device: Box::default(),
            },
            SensorSource::AtaSmart { path } => SensorBackend::AtaSmart {
                path,
                device: backends::AtaSmartSensor::default(),
            },
        };
        Ok(Self {
            config,
            width,
            no_disk_wakeup,
            backend,
        })
    }
}
fn thermal(path: &std::path::Path) -> Result<Vec<f64>> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let values = text
        .trim()
        .strip_prefix("temperatures:")
        .context("expected 'temperatures:' in ThinkPad thermal interface")?;
    values
        .split_whitespace()
        .map(|v| v.parse::<f64>().context("invalid ThinkPad temperature"))
        .collect()
}
impl SensorDevice for ConfiguredSensor {
    fn width(&self) -> usize {
        self.width
    }
    fn read(&mut self) -> Result<Vec<Option<f64>>> {
        self.read_in_session(None)
    }
}
impl ConfiguredSensor {
    pub(crate) fn is_lm(&self) -> bool {
        matches!(self.backend, SensorBackend::LmSensors { .. })
    }
    pub fn generation(&self) -> u64 {
        match &self.backend {
            SensorBackend::Hwmon(s) => s.generation,
            _ => 0,
        }
    }
    pub fn invalidate(&mut self) {
        if let SensorBackend::Hwmon(s) = &mut self.backend {
            s.invalidate();
        }
    }
    pub(crate) fn read_in_session(
        &mut self,
        session: Option<&backends::LmSession>,
    ) -> Result<Vec<Option<f64>>> {
        let backend = match &self.backend {
            SensorBackend::Hwmon(_) => "hwmon",
            SensorBackend::ThinkPad { .. } => "thinkpad",
            SensorBackend::LmSensors { .. } => "libsensors",
            SensorBackend::Nvml { .. } => "nvml",
            SensorBackend::AtaSmart { .. } => "smart",
        };
        let _span = tracing::trace_span!("sensor_read", backend).entered();
        let _measure = crate::metrics::Measure::start(crate::metrics::Operation::SensorRead);
        let s = &self.config;
        let raw: Vec<Option<f64>> = match &mut self.backend {
            SensorBackend::Hwmon(device) => device.read()?,
            SensorBackend::ThinkPad { path, indices } => {
                let values = thermal(path)?;
                let selected = if let Some(indices) = indices {
                    indices
                        .iter()
                        .map(|i| {
                            values
                                .get(i - 1)
                                .copied()
                                .context("tpacpi index exceeds channel count")
                        })
                        .collect::<Result<Vec<_>>>()?
                } else {
                    values
                };
                selected
                    .into_iter()
                    .map(|t| if t == -128.0 { None } else { Some(t) })
                    .collect()
            }
            SensorBackend::LmSensors { chip, ids } => match session {
                Some(session) => session.read(chip, ids)?,
                None => backends::LmSession::open()?.read(chip, ids)?,
            }
            .into_iter()
            .map(Some)
            .collect(),
            SensorBackend::Nvml { bus, device } => vec![Some(device.read(bus)?)],
            SensorBackend::AtaSmart { path, device } => {
                vec![device.read(path, self.no_disk_wakeup)?]
            }
        };
        ensure!(
            raw.len() == self.width,
            "sensor channel count changed: expected {}, got {}",
            self.width,
            raw.len()
        );
        raw.into_iter()
            .enumerate()
            .map(|(i, value)| {
                value
                    .map(|t| {
                        ensure!(
                            t.is_finite() && (-273.15..=1000.0).contains(&t),
                            "invalid temperature {t}"
                        );
                        let corrected = t + s.correction.get(i).copied().unwrap_or(0.0);
                        ensure!(corrected.is_finite(), "temperature correction overflow");
                        Ok(corrected)
                    })
                    .transpose()
            })
            .collect()
    }
}
