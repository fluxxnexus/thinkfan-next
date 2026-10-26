//! Configuration constraints checked before hardware activation.
use super::{Config, FanConfig, SensorConfig};
use anyhow::{Context, Result, ensure};
use std::{collections::HashSet, path::Path};
impl Config {
    pub fn validate(&self) -> Result<()> {
        seconds(self.control.down_delay_seconds)?;
        self.control.prediction.validate()?;
        ensure!(!self.sensors.is_empty(), "at least one sensor is required");
        ensure!(!self.fans.is_empty(), "at least one fan is required");
        for (i, s) in self.sensors.iter().enumerate() {
            s.validate().with_context(|| format!("sensors[{i}]"))?;
        }
        for (i, f) in self.fans.iter().enumerate() {
            f.validate().with_context(|| format!("fans[{i}]"))?;
        }
        ensure!(
            self.sensors.iter().filter(|s| s.tpacpi.is_some()).count() <= 1,
            "at most one tpacpi sensor entry is allowed"
        );
        ensure!(
            self.fans.iter().filter(|s| s.tpacpi.is_some()).count() <= 1,
            "at most one tpacpi fan entry is allowed"
        );
        if self.levels.is_some() {
            ensure!(
                self.fans.iter().all(|f| f.levels.is_none()),
                "global and per-fan levels are mutually exclusive"
            );
        } else {
            ensure!(
                self.fans.iter().all(|f| f.levels.is_some()),
                "each fan needs levels when global levels are absent"
            );
        }
        // Unknown tpacpi width is resolved by a read-only probe later. Still validate
        // all curves now, including matching detailed widths with one another.
        let count: Option<usize> = self.sensors.iter().map(SensorConfig::width).sum();
        self.curves(count)?;
        Ok(())
    }
}

fn seconds(value: f64) -> Result<()> {
    ensure!(
        value.is_finite() && (0.0..=86400.0).contains(&value),
        "duration must be finite and within 0..86400 seconds"
    );
    Ok(())
}

fn nonempty(value: &str) -> Result<()> {
    ensure!(
        !value.trim().is_empty() && !value.contains('\0'),
        "device identifiers must be nonempty and contain no NUL"
    );
    Ok(())
}
fn indices(indices: &Option<Vec<usize>>) -> Result<()> {
    if let Some(v) = indices {
        ensure!(!v.is_empty(), "indices must not be empty");
        ensure!(v.iter().all(|i| *i > 0), "indices are one-based");
        ensure!(
            v.iter().collect::<HashSet<_>>().len() == v.len(),
            "duplicate indices"
        );
    }
    Ok(())
}
fn hwmon_options(
    path: &Path,
    name: &Option<String>,
    model: &Option<String>,
    ix: &Option<Vec<usize>>,
) -> Result<()> {
    ensure!(path.is_absolute(), "hwmon path must be absolute");
    if name.is_some() || model.is_some() {
        ensure!(ix.is_some(), "name/model lookup requires indices");
    }
    for s in name.iter().chain(model.iter()) {
        nonempty(s)?;
    }
    indices(ix)
}
impl SensorConfig {
    pub fn width(&self) -> Option<usize> {
        if self.chip.is_some() {
            self.ids.as_ref().map(Vec::len)
        } else if let Some(ix) = &self.indices {
            Some(ix.len())
        } else if self.tpacpi.is_some() {
            None
        } else {
            Some(1)
        }
    }
    pub(crate) fn validate(&self) -> Result<()> {
        for (name, value) in [
            ("interval_seconds", self.interval_seconds),
            ("max_age_seconds", self.max_age_seconds),
            ("read_timeout_seconds", self.read_timeout_seconds),
        ] {
            if let Some(value) = value {
                ensure!(
                    value.is_finite() && (0.1..=86400.0).contains(&value),
                    "{name} must be within 0.1..86400 seconds"
                );
            }
        }
        ensure!(
            self.max_age_seconds.is_none() || self.interval_seconds.is_some(),
            "max_age_seconds requires interval_seconds"
        );
        if let (Some(interval), Some(age)) = (self.interval_seconds, self.max_age_seconds) {
            ensure!(
                age > interval,
                "max_age_seconds must exceed interval_seconds"
            );
        }
        ensure!(
            [
                self.hwmon.is_some(),
                self.tpacpi.is_some(),
                self.chip.is_some(),
                self.nvml.is_some(),
                self.atasmart.is_some()
            ]
            .into_iter()
            .filter(|x| *x)
            .count()
                == 1,
            "specify exactly one of hwmon, tpacpi, chip, nvml, atasmart"
        );
        indices(&self.indices)?;
        if let Some(p) = &self.hwmon {
            hwmon_options(p, &self.name, &self.model, &self.indices)?;
        } else {
            ensure!(
                self.name.is_none() && self.model.is_none(),
                "name/model require hwmon"
            );
        }
        if self.hwmon.is_none() && self.tpacpi.is_none() {
            ensure!(self.indices.is_none(), "indices require hwmon or tpacpi");
        }
        if let Some(chip) = &self.chip {
            nonempty(chip)?;
            let ids = self.ids.as_ref().context("chip requires ids")?;
            ensure!(!ids.is_empty(), "ids must not be empty");
            ensure!(
                ids.iter().collect::<HashSet<_>>().len() == ids.len(),
                "duplicate sensor ids"
            );
            for id in ids {
                nonempty(id)?;
            }
        } else {
            ensure!(self.ids.is_none(), "ids require chip");
        }
        if let Some(id) = &self.nvml {
            nonempty(id)?;
        }
        for p in self.tpacpi.iter().chain(self.atasmart.iter()) {
            ensure!(p.is_absolute(), "device paths must be absolute");
        }
        ensure!(
            self.correction.iter().all(|x| x.is_finite()),
            "corrections must be finite"
        );
        if let Some(n) = self.width() {
            self.validate_width(n)?;
        }
        Ok(())
    }
    pub fn validate_width(&self, n: usize) -> Result<()> {
        ensure!(n > 0, "sensor returned no channels");
        ensure!(
            self.correction.is_empty() || self.correction.len() == n,
            "correction length must equal the sensor channel count ({n})"
        );
        Ok(())
    }
}
impl FanConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        if let Some(delay) = self.down_delay_seconds {
            seconds(delay)?;
        }
        ensure!(
            self.hwmon.is_some() != self.tpacpi.is_some(),
            "specify exactly one of hwmon or tpacpi"
        );
        if let Some(p) = &self.hwmon {
            hwmon_options(p, &self.name, &self.model, &self.indices)?;
            ensure!(
                self.indices.as_ref().is_none_or(|v| v.len() == 1),
                "each fan entry must select exactly one PWM channel"
            );
        } else {
            ensure!(
                self.name.is_none() && self.model.is_none() && self.indices.is_none(),
                "tpacpi fans do not accept hwmon selectors"
            );
            ensure!(
                self.tpacpi.as_ref().unwrap().is_absolute(),
                "tpacpi path must be absolute"
            );
        }
        Ok(())
    }
}
