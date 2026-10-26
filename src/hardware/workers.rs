//! Fixed process-lifetime read workers. No hardware writes are submitted here.
use anyhow::Result;
use mio::Waker;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender},
    },
    thread,
    time::{Duration, Instant},
};
type Job = Box<dyn FnOnce() + Send + 'static>;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WorkClass {
    Fast,
    Gpu,
    Disk,
    Prepare,
    Probe,
}
#[derive(Default)]
struct Timing {
    started: Option<Instant>,
    completed: u64,
    total: Duration,
    maximum: Duration,
}
struct Worker {
    class: WorkClass,
    sender: SyncSender<Job>,
    busy: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,
    timing: Arc<Mutex<Timing>>,
}
pub(crate) struct ReadWorkers {
    workers: Vec<Worker>,
    waker: Arc<Waker>,
}
impl ReadWorkers {
    pub fn new(waker: Arc<Waker>) -> Result<Self> {
        let mut workers = Vec::new();
        for (index, class) in [
            WorkClass::Fast,
            WorkClass::Fast,
            WorkClass::Gpu,
            WorkClass::Disk,
            WorkClass::Prepare,
            WorkClass::Probe,
        ]
        .into_iter()
        .enumerate()
        {
            let (sender, receiver) = mpsc::sync_channel::<Job>(1);
            let busy = Arc::new(AtomicBool::new(false));
            let running = busy.clone();
            let failed = Arc::new(AtomicBool::new(false));
            let failure = failed.clone();
            let timing = Arc::new(Mutex::new(Timing::default()));
            let measured = timing.clone();
            let wake = waker.clone();
            thread::Builder::new()
                .name(format!("sensor-read-{index}"))
                .spawn(move || {
                    while let Ok(job) = receiver.recv() {
                        // Jobs convert backend panics into failed acquisition results.
                        let measure =
                            crate::metrics::Measure::start(crate::metrics::Operation::WorkerBusy);
                        let started = Instant::now();
                        measured.lock().unwrap().started = Some(started);
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
                        let elapsed = started.elapsed();
                        {
                            let mut timing = measured.lock().unwrap();
                            timing.started = None;
                            timing.completed += 1;
                            timing.total += elapsed;
                            timing.maximum = timing.maximum.max(elapsed);
                        }
                        failure.store(result.is_err(), Ordering::Release);
                        drop(measure);
                        running.store(false, Ordering::Release);
                        if let Err(error) = wake.wake() {
                            tracing::error!(%error, "sensor completion wake failed");
                        }
                        if result.is_err() {
                            break;
                        }
                    }
                })?;
            workers.push(Worker {
                sender,
                busy,
                class,
                failed,
                timing,
            });
        }
        Ok(Self { workers, waker })
    }
    pub fn check_health(&self) -> Result<()> {
        anyhow::ensure!(
            !self
                .workers
                .iter()
                .any(|w| w.failed.load(Ordering::Acquire)),
            "read worker panicked"
        );
        Ok(())
    }
    pub fn waker(&self) -> Arc<Waker> {
        self.waker.clone()
    }
    pub fn log_snapshot(&self) {
        for class in [
            WorkClass::Fast,
            WorkClass::Gpu,
            WorkClass::Disk,
            WorkClass::Prepare,
            WorkClass::Probe,
        ] {
            let active = self
                .workers
                .iter()
                .filter(|w| w.class == class && w.busy.load(Ordering::Acquire))
                .count();
            let capacity = self.workers.iter().filter(|w| w.class == class).count();
            let mut completed = 0;
            let mut total = Duration::ZERO;
            let mut maximum = Duration::ZERO;
            let mut longest_active = Duration::ZERO;
            for worker in self.workers.iter().filter(|w| w.class == class) {
                let timing = worker.timing.lock().unwrap();
                completed += timing.completed;
                total += timing.total;
                maximum = maximum.max(timing.maximum);
                if let Some(started) = timing.started {
                    longest_active = longest_active.max(started.elapsed());
                }
            }
            tracing::info!(
                ?class,
                active,
                capacity,
                completed,
                total_ms = total.as_millis() as u64,
                maximum_ms = maximum.as_millis() as u64,
                longest_active_ms = longest_active.as_millis() as u64,
                "worker occupancy"
            );
        }
        for operation in [
            crate::metrics::Operation::DispatchDelay,
            crate::metrics::Operation::ResultDelay,
            crate::metrics::Operation::ReadTimeout,
            crate::metrics::Operation::WorkerSaturated,
        ] {
            tracing::info!(?operation, measurement = ?crate::metrics::snapshot(operation), "scheduler metrics");
        }
    }
    /// Reserve a worker before transferring device ownership. No queued backlog
    /// per sensor and fixed per-class capacity across all configurations.
    /// Ok(true): accepted; Ok(false): full (make is not called); Err: worker failed.
    pub fn submit(&self, class: WorkClass, make: impl FnOnce() -> Job) -> Result<bool> {
        self.check_health()?;
        for worker in self.workers.iter().filter(|w| w.class == class) {
            if worker
                .busy
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                worker.sender.send(make()).map_err(|_| {
                    worker.failed.store(true, Ordering::Release);
                    anyhow::anyhow!("{class:?} worker disconnected")
                })?;
                return Ok(true);
            }
        }
        crate::metrics::record(
            crate::metrics::Operation::WorkerSaturated,
            std::time::Duration::ZERO,
        );
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn poll_completion(poll: &mut mio::Poll) {
        let mut events = mio::Events::with_capacity(8);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            assert!(Instant::now() < deadline, "worker did not wake the reactor");
            poll.poll(
                &mut events,
                Some(deadline.saturating_duration_since(Instant::now())),
            )
            .unwrap();
            if events.iter().any(|event| event.token() == mio::Token(1)) {
                return;
            }
        }
    }
    #[test]
    fn capacity_release_wakes_poll_and_accepts_a_new_job() {
        let mut poll = mio::Poll::new().unwrap();
        let pool = ReadWorkers::new(Arc::new(
            Waker::new(poll.registry(), mio::Token(1)).unwrap(),
        ))
        .unwrap();
        let (release, blocked) = mpsc::sync_channel(1);
        assert!(
            pool.submit(WorkClass::Disk, || Box::new(move || {
                blocked.recv().unwrap();
            }))
            .unwrap()
        );
        assert!(
            !pool
                .submit(WorkClass::Disk, || panic!("full lane took ownership"))
                .unwrap()
        );
        release.send(()).unwrap();
        poll_completion(&mut poll);
        let (completed, result) = mpsc::sync_channel(1);
        assert!(
            pool.submit(WorkClass::Disk, || Box::new(move || {
                completed.send(()).unwrap();
            }))
            .unwrap()
        );
        result.recv_timeout(Duration::from_secs(5)).unwrap();
        pool.check_health().unwrap();
    }
    #[test]
    fn worker_panic_wakes_poll_and_rejects_further_ownership_transfer() {
        let mut poll = mio::Poll::new().unwrap();
        let pool = ReadWorkers::new(Arc::new(
            Waker::new(poll.registry(), mio::Token(1)).unwrap(),
        ))
        .unwrap();
        assert!(
            pool.submit(WorkClass::Disk, || Box::new(|| panic!(
                "injected worker panic"
            )))
            .unwrap()
        );
        poll_completion(&mut poll);
        assert!(pool.check_health().is_err());
        assert!(
            pool.submit(WorkClass::Fast, || panic!("unhealthy pool took ownership"))
                .is_err()
        );
        assert_eq!(pool.workers.len(), 6);
    }
    #[test]
    fn disconnected_worker_is_an_error_not_accepted_or_full() {
        let poll = mio::Poll::new().unwrap();
        let waker = Arc::new(Waker::new(poll.registry(), mio::Token(1)).unwrap());
        let (sender, receiver) = mpsc::sync_channel(1);
        drop(receiver);
        let pool = ReadWorkers {
            waker,
            workers: vec![Worker {
                class: WorkClass::Fast,
                sender,
                busy: Arc::new(AtomicBool::new(false)),
                failed: Arc::new(AtomicBool::new(false)),
                timing: Arc::new(Mutex::new(Timing::default())),
            }],
        };
        assert!(pool.submit(WorkClass::Fast, || Box::new(|| {})).is_err());
    }
    #[test]
    fn blocked_maintenance_and_slow_jobs_leave_fast_capacity_available() {
        let poll = mio::Poll::new().unwrap();
        let pool = ReadWorkers::new(Arc::new(
            Waker::new(poll.registry(), mio::Token(1)).unwrap(),
        ))
        .unwrap();
        let mut releases = Vec::new();
        for class in [
            WorkClass::Gpu,
            WorkClass::Disk,
            WorkClass::Prepare,
            WorkClass::Probe,
        ] {
            let (send, receive) = mpsc::sync_channel(1);
            releases.push(send);
            assert!(
                pool.submit(class, || Box::new(move || {
                    receive.recv().unwrap();
                }))
                .unwrap()
            );
            assert!(
                !pool
                    .submit(class, || panic!(
                        "saturated lane must not transfer ownership"
                    ))
                    .unwrap()
            );
        }
        let (send, receive) = mpsc::sync_channel(1);
        assert!(
            pool.submit(WorkClass::Fast, || Box::new(move || {
                send.send(()).unwrap();
            }))
            .unwrap()
        );
        receive
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        for release in releases {
            release.send(()).unwrap();
        }
    }
}
