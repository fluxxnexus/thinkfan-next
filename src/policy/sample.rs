use std::time::Duration;
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SampleState {
    Value(f64),
    Sleeping,
    Missing,
    ReadError,
    Unavailable,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Availability {
    Fresh,
    Expired,
    Sleeping,
    Missing,
    ReadError,
    Unavailable,
}
#[derive(Clone, Copy, Debug)]
pub struct Sample {
    pub state: SampleState,
    pub captured_at: Duration,
    pub valid_until: Duration,
    pub sequence: u64,
    pub generation: u64,
}
impl Default for Sample {
    fn default() -> Self {
        Self {
            state: SampleState::Unavailable,
            captured_at: Duration::ZERO,
            valid_until: Duration::ZERO,
            sequence: 0,
            generation: 0,
        }
    }
}
impl Sample {
    pub fn availability(&self, now: Duration) -> Availability {
        match self.state {
            SampleState::Value(value) if !value.is_finite() => Availability::ReadError,
            SampleState::Value(_) if now < self.captured_at || now >= self.valid_until => {
                Availability::Expired
            }
            SampleState::Value(_) => Availability::Fresh,
            SampleState::Sleeping => Availability::Sleeping,
            SampleState::Missing => Availability::Missing,
            SampleState::ReadError => Availability::ReadError,
            SampleState::Unavailable => Availability::Unavailable,
        }
    }
    pub fn value(&self, now: Duration) -> Option<f64> {
        if self.availability(now) == Availability::Fresh
            && let SampleState::Value(t) = self.state
        {
            return Some(t);
        }
        None
    }
}
