//! Cumulative process-local measurements. No hardware is accessed by snapshots.
use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};
#[derive(Clone, Copy, Debug)]
#[repr(usize)]
pub enum Operation {
    HwmonDiscovery,
    SensorRead,
    LmInit,
    SmartLoad,
    Cycle,
    DispatchDelay,
    ResultDelay,
    ReadTimeout,
    WorkerBusy,
    WorkerSaturated,
    FanProbe,
    FanApply,
}
const COUNT: usize = 12;
static CALLS: [AtomicU64; COUNT] = [const { AtomicU64::new(0) }; COUNT];
static NANOS: [AtomicU64; COUNT] = [const { AtomicU64::new(0) }; COUNT];
static MAX_NANOS: [AtomicU64; COUNT] = [const { AtomicU64::new(0) }; COUNT];
#[derive(Clone, Copy, Debug)]
pub struct Measurement {
    pub calls: u64,
    pub total_ns: u64,
    pub max_ns: u64,
}
pub fn snapshot(operation: Operation) -> Measurement {
    let i = operation as usize;
    Measurement {
        calls: CALLS[i].load(Ordering::Relaxed),
        total_ns: NANOS[i].load(Ordering::Relaxed),
        max_ns: MAX_NANOS[i].load(Ordering::Relaxed),
    }
}
pub struct Measure {
    operation: Operation,
    start: Instant,
}
impl Measure {
    pub fn start(operation: Operation) -> Self {
        Self {
            operation,
            start: Instant::now(),
        }
    }
}
pub fn record(operation: Operation, elapsed: Duration) {
    let ns = elapsed.as_nanos().min(u64::MAX as u128) as u64;
    let i = operation as usize;
    CALLS[i].fetch_add(1, Ordering::Relaxed);
    NANOS[i].fetch_add(ns, Ordering::Relaxed);
    MAX_NANOS[i].fetch_max(ns, Ordering::Relaxed);
    tracing::trace!(?operation, elapsed_ns = ns, "hardware/control timing");
}
impl Drop for Measure {
    fn drop(&mut self) {
        record(self.operation, self.start.elapsed());
    }
}
