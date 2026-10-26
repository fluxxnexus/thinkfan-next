//! Time-based sampling recovery driven by fresh acquisition evidence.
use crate::policy::sample::Sample;
use std::time::Duration;

pub(super) struct AdaptiveSampling {
    base: Duration,
    interval: Duration,
    seen: Vec<Option<(u64, u64)>>,
    anchor: Option<(Duration, Duration)>,
    observed: Option<Duration>,
    evaluated: Option<Duration>,
}
impl AdaptiveSampling {
    pub fn new(base: Duration, channels: usize) -> Self {
        Self {
            base,
            interval: base,
            seen: vec![None; channels],
            anchor: None,
            observed: None,
            evaluated: None,
        }
    }
    pub fn interval(&self) -> Duration {
        self.interval
    }
    pub fn interrupt(&mut self) {
        self.anchor = None;
    }
    pub fn reset(&mut self) {
        self.interval = self.base;
        self.seen.fill(None);
        self.anchor = None;
        self.observed = None;
        self.evaluated = None;
    }
    pub fn update(
        &mut self,
        samples: &[Sample],
        now: Duration,
        rising: bool,
        fresh_rising: bool,
        healthy: bool,
    ) {
        if self.evaluated.is_some_and(|last| now < last) {
            self.reset();
        }
        self.evaluated = Some(now);
        let mut evidence = None;
        let mut replaced = false;
        for (seen, sample) in self.seen.iter_mut().zip(samples) {
            let key = (sample.sequence, sample.generation);
            if *seen != Some(key) && sample.value(now).is_some() {
                evidence = Some(evidence.map_or(sample.captured_at, |at: Duration| {
                    at.max(sample.captured_at)
                }));
                replaced |= seen.is_some_and(|(_, generation)| generation != sample.generation);
            }
            *seen = Some(key);
        }
        if !healthy || replaced {
            // No recovery credit for time spent without usable control evidence.
            self.anchor = None;
        }
        let Some(evidence) = evidence else {
            return;
        };
        let at = self.observed.map_or(evidence, |last| last.max(evidence));
        self.observed = Some(at);
        if rising {
            // Cached rising channels hold the fast cadence without moving the
            // recovery origin when another channel finishes.
            if !fresh_rising {
                return;
            }
            self.interval = self.base.min(Duration::from_secs(2));
            self.anchor = healthy.then_some((at, self.interval));
        } else if healthy {
            let (start, initial) = *self.anchor.get_or_insert((at, self.interval));
            // One second of recovery per base interval. Compute from an anchor
            // rather than repeatedly rounding increments for each completion.
            let recovered = Duration::from_secs_f64(
                (at.saturating_sub(start).as_secs_f64() / self.base.as_secs_f64())
                    .min(self.base.as_secs_f64()),
            );
            self.interval = initial.saturating_add(recovered).min(self.base);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::sample::SampleState;
    fn sample(at: f64, sequence: u64) -> Sample {
        let captured_at = Duration::from_secs_f64(at);
        Sample {
            state: SampleState::Value(40.0),
            captured_at,
            valid_until: captured_at + Duration::from_secs(30),
            sequence,
            generation: 0,
        }
    }
    fn update(state: &mut AdaptiveSampling, at: f64, sequence: u64, rising: bool) {
        state.update(
            &[sample(at, sequence)],
            Duration::from_secs_f64(at),
            rising,
            rising,
            true,
        );
    }

    #[test]
    fn actual_policy_cached_rise_does_not_restart_sampling_recovery() {
        use crate::{
            config::Config,
            policy::{DecisionReason, PolicyEngine},
        };
        let config = Config::parse("sensors: [{hwmon: /cpu}, {hwmon: /gpu}]\nfans: [{hwmon: /pwm}]\nlevels: [[0, 0, 50], [180, 45, 120]]").unwrap();
        let plan = config.compile(&[1, 1]).unwrap();
        let run = |split: bool| {
            let mut policy = PolicyEngine::new(&plan, 0.0);
            let mut cadence = AdaptiveSampling::new(Duration::from_secs(5), 2);
            let mut inputs = [sample(0.0, 1), sample(0.0, 1)];
            let apply = |policy: &mut PolicyEngine,
                         cadence: &mut AdaptiveSampling,
                         samples: &[Sample],
                         now| {
                policy.evaluate(samples, now, false).unwrap();
                cadence.update(
                    samples,
                    now,
                    policy.decision.rising,
                    policy.fresh_rising(),
                    policy.decision.reason == DecisionReason::Curve,
                );
            };
            apply(&mut policy, &mut cadence, &inputs, Duration::ZERO);
            inputs = [sample(1.0, 2), sample(1.0, 2)];
            for input in &mut inputs {
                input.state = SampleState::Value(50.0);
            }
            apply(&mut policy, &mut cadence, &inputs, Duration::from_secs(1));
            assert_eq!(cadence.interval(), Duration::from_secs(2));
            inputs[0] = sample(6.0, 3);
            if split {
                apply(&mut policy, &mut cadence, &inputs, Duration::from_secs(6));
                assert!(policy.decision.rising);
                assert!(!policy.fresh_rising());
                assert_eq!(cadence.interval(), Duration::from_secs(2));
            }
            inputs[1] = sample(6.0, 3);
            apply(&mut policy, &mut cadence, &inputs, Duration::from_secs(6));
            assert!(!policy.decision.rising);
            cadence.interval()
        };
        assert_eq!(run(true), Duration::from_secs(3));
        assert_eq!(run(false), Duration::from_secs(3));
    }
    #[test]
    fn recovery_depends_on_elapsed_time_not_completion_count() {
        let mut frequent = AdaptiveSampling::new(Duration::from_secs(5), 1);
        let mut sparse = AdaptiveSampling::new(Duration::from_secs(5), 1);
        update(&mut frequent, 0.0, 1, true);
        update(&mut sparse, 0.0, 1, true);
        for i in 1..=100 {
            update(&mut frequent, i as f64 / 10.0, i + 1, false);
        }
        update(&mut sparse, 10.0, 2, false);
        assert_eq!(frequent.interval(), Duration::from_secs(4));
        assert_eq!(frequent.interval(), sparse.interval());
        update(&mut sparse, 30.0, 3, false);
        assert_eq!(sparse.interval(), Duration::from_secs(5));
        update(&mut sparse, 31.0, 4, true);
        assert_eq!(sparse.interval(), Duration::from_secs(2));
    }
    #[test]
    fn split_and_batched_completions_have_identical_recovery() {
        let mut split = AdaptiveSampling::new(Duration::from_secs(5), 2);
        let mut batch = AdaptiveSampling::new(Duration::from_secs(5), 2);
        let initial = [sample(0.0, 1), sample(0.0, 1)];
        for state in [&mut split, &mut batch] {
            state.update(&initial, Duration::ZERO, true, true, true);
        }
        split.update(
            &[sample(5.0, 2), initial[1]],
            Duration::from_secs(5),
            true,
            false,
            true,
        );
        split.update(
            &[sample(5.0, 2), sample(5.0, 2)],
            Duration::from_secs(5),
            false,
            false,
            true,
        );
        batch.update(
            &[sample(5.0, 2), sample(5.0, 2)],
            Duration::from_secs(5),
            false,
            false,
            true,
        );
        assert_eq!(split.interval(), Duration::from_secs(3));
        assert_eq!(split.interval(), batch.interval());
    }
    #[test]
    fn cached_expired_and_failed_samples_do_not_advance_recovery() {
        let mut state = AdaptiveSampling::new(Duration::from_secs(5), 1);
        update(&mut state, 0.0, 1, true);
        for at in [1, 5, 10] {
            state.update(
                &[sample(0.0, 1)],
                Duration::from_secs(at),
                false,
                false,
                true,
            );
        }
        assert_eq!(state.interval(), Duration::from_secs(2));
        state.update(
            &[sample(1.0, 2)],
            Duration::from_secs(40),
            false,
            false,
            false,
        );
        update(&mut state, 50.0, 3, false);
        assert_eq!(state.interval(), Duration::from_secs(2));
        update(&mut state, 55.0, 4, false);
        assert_eq!(state.interval(), Duration::from_secs(3));
    }
    #[test]
    fn reset_replacement_and_time_reversal_discard_recovery_credit() {
        let mut state = AdaptiveSampling::new(Duration::from_secs(5), 1);
        update(&mut state, 0.0, 1, true);
        let mut replaced = sample(20.0, 2);
        replaced.generation = 1;
        state.update(&[replaced], Duration::from_secs(20), false, false, true);
        assert_eq!(state.interval(), Duration::from_secs(2));
        state.reset();
        assert_eq!(state.interval(), Duration::from_secs(5));
        update(&mut state, 100.0, 1, true);
        assert_eq!(state.interval(), Duration::from_secs(2));
        update(&mut state, 1.0, 2, false);
        assert_eq!(state.interval(), Duration::from_secs(5));
    }
    #[test]
    fn recovery_uses_capture_time_and_never_slows_a_short_base_interval() {
        let mut state = AdaptiveSampling::new(Duration::from_secs(5), 1);
        update(&mut state, 0.0, 1, true);
        state.update(
            &[sample(1.0, 2)],
            Duration::from_secs(10),
            false,
            false,
            true,
        );
        assert_eq!(state.interval(), Duration::from_millis(2200));
        let mut fast = AdaptiveSampling::new(Duration::from_millis(100), 1);
        update(&mut fast, 0.0, 1, true);
        update(&mut fast, 1.0, 2, false);
        assert_eq!(fast.interval(), Duration::from_millis(100));
    }
}
