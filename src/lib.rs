#![cfg_attr(not(target_os = "linux"), allow(unused))]
#[cfg(not(target_os = "linux"))]
compile_error!("thinkfan-next requires Linux");

pub mod config;
pub mod hardware;
pub mod metrics;
pub mod policy;
pub mod runtime;

// Compatibility aliases for existing library consumers.
pub use hardware as drivers;
pub use policy::control;
