//! Scheduling and orchestration; hardware ownership and policy are separate.
mod adaptive;
mod controller;
mod diagnostics;
mod event_loop;
mod options;
mod reactor;
mod schedule;
pub use controller::Controller;
pub use event_loop::{acquire_lock, run};
pub use options::Options;
