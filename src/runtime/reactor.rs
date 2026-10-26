//! Signals are delivered via signal-hook's self-pipe, registered with mio.
//! Read workers notify the same poll through a Waker; fan writes stay on the loop.
use mio::{Events, Interest, Poll, Token, Waker};
use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGTERM, SIGUSR1, SIGUSR2};
use signal_hook_mio::v1_0::Signals;
use std::{io, sync::Arc, time::Instant};

const SIGNALS: Token = Token(0);
const READS: Token = Token(1);

/// Standard signals may coalesce and arrive in arbitrary order. Preserve the
/// established priority: termination, sleep, wake, reload, then diagnostics.
#[derive(Default, Debug, PartialEq, Eq)]
pub(crate) struct SignalBatch {
    pub stop: bool,
    pub sleep: bool,
    pub wake: bool,
    pub reload: bool,
    pub dump: bool,
    pub work_ready: bool,
}
impl SignalBatch {
    fn collect(signals: impl Iterator<Item = i32>) -> Self {
        let mut batch = Self::default();
        for signal in signals {
            tracing::trace!(signal, "signal received");
            match signal {
                SIGINT | SIGTERM => batch.stop = true,
                libc::SIGPWR => batch.sleep = true,
                SIGUSR2 => batch.wake = true,
                SIGHUP => batch.reload = true,
                SIGUSR1 => batch.dump = true,
                _ => (),
            }
        }
        batch
    }
    fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

pub(crate) struct Reactor {
    // Keep the poll alive until after its registered signal source is dropped.
    signals: Signals,
    poll: Poll,
    events: Events,
    pub waker: Arc<Waker>,
    deferred: SignalBatch,
}
impl Reactor {
    pub fn new() -> io::Result<Self> {
        let poll = Poll::new()?;
        let mut signals = Signals::new([SIGINT, SIGTERM, SIGHUP, SIGUSR1, libc::SIGPWR, SIGUSR2])?;
        poll.registry()
            .register(&mut signals, SIGNALS, Interest::READABLE)?;
        let waker = Arc::new(Waker::new(poll.registry(), READS)?);
        Ok(Self {
            waker,
            deferred: SignalBatch::default(),
            signals,
            poll,
            events: Events::with_capacity(8),
        })
    }

    pub fn defer(&mut self, batch: SignalBatch) {
        self.deferred = batch;
    }

    /// None means wait indefinitely (sleep state); an expired deadline performs
    /// a nonblocking signal drain. Spurious wakes/EINTR never reset a deadline.
    pub fn wait(&mut self, deadline: Option<Instant>) -> io::Result<SignalBatch> {
        if !self.deferred.is_empty() {
            return Ok(std::mem::take(&mut self.deferred));
        }
        loop {
            // Also drain signals that arrived during synchronous hardware work,
            // or interrupted poll before an event was returned. pending() drains
            // the self-pipe so mio's edge-triggered readiness cannot get stuck.
            let batch = SignalBatch::collect(self.signals.pending());
            if !batch.is_empty() {
                return Ok(batch);
            }
            let timeout = deadline.map(|end| end.saturating_duration_since(Instant::now()));
            if timeout.is_some_and(|t| t.is_zero()) {
                return Ok(batch);
            }
            match self.poll.poll(&mut self.events, timeout) {
                Ok(()) => {
                    if self.events.iter().any(|event| event.token() == READS) {
                        let mut batch = SignalBatch::collect(self.signals.pending());
                        batch.work_ready = true;
                        return Ok(batch);
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn simultaneous_signals_are_coalesced_without_losing_actions() {
        let batch = SignalBatch::collect(
            [
                SIGUSR1,
                SIGUSR2,
                SIGHUP,
                libc::SIGPWR,
                SIGINT,
                SIGTERM,
                SIGHUP,
            ]
            .into_iter(),
        );
        assert_eq!(
            batch,
            SignalBatch {
                stop: true,
                sleep: true,
                wake: true,
                reload: true,
                work_ready: false,
                dump: true
            }
        );
    }
}
