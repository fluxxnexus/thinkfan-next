//! Hardware effects, isolated from control policy.
mod backends;
mod fans;
mod sensors;
mod workers;
pub use backends::hwmon::resolve_hwmon;
pub use fans::{FanDevice, FanManager, FileFan, ReloadPlan};
pub use sensors::{ConfiguredSensor, SensorDevice, SensorHub};
pub(crate) use workers::{ReadWorkers, WorkClass};
