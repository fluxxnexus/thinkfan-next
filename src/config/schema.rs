//! Strict YAML input types.
use crate::policy::control::{Prediction, Speed};
use serde::Deserialize;
use std::path::PathBuf;
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub control: ControlConfig,
    pub sensors: Vec<SensorConfig>,
    pub fans: Vec<FanConfig>,
    pub levels: Option<Vec<LevelConfig>>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SensorConfig {
    pub interval_seconds: Option<f64>,
    pub max_age_seconds: Option<f64>,
    pub read_timeout_seconds: Option<f64>,
    pub hwmon: Option<PathBuf>,
    pub tpacpi: Option<PathBuf>,
    pub chip: Option<String>,
    pub nvml: Option<String>,
    pub atasmart: Option<PathBuf>,
    pub name: Option<String>,
    pub model: Option<String>,
    pub indices: Option<Vec<usize>>,
    pub ids: Option<Vec<String>>,
    #[serde(default)]
    pub correction: Vec<f64>,
    #[serde(default)]
    pub optional: bool,
    #[serde(default)]
    pub max_errors: u32,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FanConfig {
    pub down_delay_seconds: Option<f64>,
    pub hwmon: Option<PathBuf>,
    pub tpacpi: Option<PathBuf>,
    pub name: Option<String>,
    pub model: Option<String>,
    pub indices: Option<Vec<usize>>,
    #[serde(default)]
    pub optional: bool,
    #[serde(default)]
    pub max_errors: u32,
    pub levels: Option<Vec<LevelConfig>>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ControlConfig {
    pub down_delay_seconds: f64,
    pub prediction: Prediction,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum Speeds {
    One(Speed),
    Many(Vec<Speed>),
}
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum Bound {
    Temperature(f64),
    Ignored(String),
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DetailedLevel {
    pub speed: Speeds,
    pub lower_limit: Option<Vec<Bound>>,
    pub upper_limit: Option<Vec<Bound>>,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum LevelConfig {
    Simple((Speeds, f64, f64)),
    Detailed(DetailedLevel),
}
