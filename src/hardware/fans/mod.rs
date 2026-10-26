//! Fan ownership, command execution and recovery.
mod device;
mod manager;
pub use device::{FanDevice, FileFan};
pub use manager::{FanManager, ReloadPlan};
