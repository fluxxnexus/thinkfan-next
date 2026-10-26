//! Pure control deadlines; all time is supplied by the event loop.
use std::time::Instant;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Phase {
    Sampling(Instant),
    Depulsing(Instant),
    Paused,
}
pub(super) struct ControlSchedule {
    pub phase: Phase,
    pub after_depulse: Option<Instant>,
    pub next_maintenance: Instant,
}
impl ControlSchedule {
    pub fn new(now: Instant, paused: bool) -> Self {
        Self {
            phase: if paused {
                Phase::Paused
            } else {
                Phase::Sampling(now)
            },
            after_depulse: None,
            next_maintenance: now,
        }
    }
    pub fn deadline(&self, external: Option<Instant>) -> Option<Instant> {
        let phase = match self.phase {
            Phase::Sampling(at) | Phase::Depulsing(at) => Some(at),
            Phase::Paused => None,
        };
        match (phase, external) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
    pub fn work_ready(&mut self, now: Instant) {
        match self.phase {
            Phase::Sampling(at) => self.phase = Phase::Sampling(at.min(now)),
            Phase::Depulsing(_) => {
                self.after_depulse = Some(self.after_depulse.map_or(now, |at| at.min(now)))
            }
            Phase::Paused => (),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    #[test]
    fn reload_deadline_preempts_sampling_and_depulse_without_postponing_them() {
        let now = Instant::now();
        let soon = now + Duration::from_secs(1);
        let later = now + Duration::from_secs(5);
        for phase in [Phase::Sampling(soon), Phase::Depulsing(soon)] {
            let mut schedule = ControlSchedule::new(now, false);
            schedule.phase = phase;
            assert_eq!(schedule.deadline(Some(later)), Some(soon));
            assert_eq!(schedule.deadline(Some(now)), Some(now));
            assert_eq!(schedule.phase, phase);
        }
    }
    #[test]
    fn capacity_wake_preserves_pause_depulse_and_maintenance() {
        let now = Instant::now();
        let future = now + Duration::from_secs(5);
        let mut schedule = ControlSchedule::new(now, false);
        schedule.next_maintenance = future;
        schedule.phase = Phase::Sampling(future);
        schedule.work_ready(now);
        assert_eq!(schedule.deadline(None), Some(now));
        assert_eq!(schedule.next_maintenance, future);
        schedule.phase = Phase::Depulsing(future);
        schedule.work_ready(now);
        assert_eq!(schedule.deadline(None), Some(future));
        assert_eq!(schedule.after_depulse, Some(now));
        schedule.phase = Phase::Paused;
        schedule.work_ready(now);
        assert_eq!(schedule.deadline(None), None);
        assert_eq!(schedule.deadline(Some(future)), Some(future));
    }
}
