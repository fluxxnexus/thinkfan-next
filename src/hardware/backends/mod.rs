//! Device-specific access and native library lifetimes.
pub(super) mod hwmon;
mod lm_sensors;
mod nvml;
mod smart;
pub(super) use lm_sensors::LmSession;
pub(super) use nvml::NvmlSensor;
pub(super) use smart::AtaSmartSensor;
