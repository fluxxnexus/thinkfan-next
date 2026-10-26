//! Sensor ownership, acquisition and sample publication.
mod device;
mod hub;
pub use device::{ConfiguredSensor, SensorDevice};
pub use hub::SensorHub;
