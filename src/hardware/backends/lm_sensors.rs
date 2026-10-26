//! Serialized libsensors sessions using the safe lm-sensors API.
use anyhow::{Context, Result, ensure};
use std::{path::Path, sync::Mutex};
static LM_LOCK: Mutex<()> = Mutex::new(());
#[derive(Debug)]
struct SensorErrors;
impl lm_sensors::errors::Listener for SensorErrors {
    fn on_lm_sensors_config_error(&self, error: &str, file: Option<&Path>, line: usize) {
        tracing::error!(error, file = ?file, line, "libsensors configuration error");
    }

    fn on_lm_sensors_fatal_error(&self, error: &str, procedure: &str) {
        tracing::error!(error, procedure, "libsensors fatal error");
    }
}

pub struct LmSession {
    sensors: lm_sensors::LMSensors,
    _lock: std::sync::MutexGuard<'static, ()>,
}
impl LmSession {
    pub fn open() -> Result<Self> {
        // Serialize the entire context lifetime: lm-sensors permits one active
        // context per process. Borrowed chips/features cannot outlive this context.
        let _lock = LM_LOCK
            .lock()
            .map_err(|_| anyhow::anyhow!("libsensors lock poisoned"))?;
        let _measure = crate::metrics::Measure::start(crate::metrics::Operation::LmInit);
        let sensors = lm_sensors::Initializer::default()
            .error_listener(Box::new(SensorErrors))
            .initialize()
            .context("libsensors initialization failed")?;
        drop(_measure);
        Ok(Self { sensors, _lock })
    }
    pub fn read(&self, chip_name: &str, ids: &[String]) -> Result<Vec<f64>> {
        let sensors = &self.sensors;
        let mut selected = None;
        for chip in sensors.chip_iter(None) {
            if chip.name().context("libsensors chip name read failed")? == chip_name {
                selected = Some(chip);
                break;
            }
        }
        let selected =
            selected.with_context(|| format!("libsensors chip '{chip_name}' not found"))?;
        let mut values = vec![None; ids.len()];
        for feature in selected.feature_iter() {
            let id = feature
                .raw_label()
                .context("libsensors label read failed")?
                .to_string_lossy()
                .into_owned();
            if let Some(index) = ids.iter().position(|v| v == &id) {
                let sub = feature
                    .sub_feature_by_kind(lm_sensors::value::Kind::TemperatureInput)
                    .with_context(|| format!("'{id}' is not a temperature input"))?;
                let reading = sub
                    .raw_value()
                    .with_context(|| format!("reading libsensors feature '{id}' failed"))?;
                ensure!(values[index].is_none(), "ambiguous libsensors label '{id}'");
                values[index] = Some(reading);
            }
        }
        values
            .into_iter()
            .zip(ids)
            .map(|(value, id)| {
                value.with_context(|| format!("libsensors feature '{id}' not found"))
            })
            .collect()
    }
}
