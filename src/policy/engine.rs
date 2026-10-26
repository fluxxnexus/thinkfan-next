//! Pure policy. Inputs include acquisition time and identity, never hardware handles.
use super::{control::*, sample::*};
use crate::config::{FanPlan, RuntimePlan};
use anyhow::{Result, ensure};
use std::time::Duration;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecisionReason {
    Curve,
    SensorFault,
    FanFault,
}
#[derive(Debug)]
pub struct Decision {
    pub commands: Vec<FanCommand>,
    pub reason: DecisionReason,
    pub rising: bool,
}
pub struct PolicyEngine {
    fans: Vec<FanPlan>,
    filter: TemperatureFilter,
    prediction: Prediction,
    bias: f64,
    seen: Vec<Option<(u64, u64)>>,
    rising: Vec<bool>,
    fresh_rising: bool,
    captured: Vec<Option<Duration>>,
    required: Vec<bool>,
    pub temperatures: Vec<Option<f64>>,
    pub effective_temperatures: Vec<Option<f64>>,
    pub decision: Decision,
}
impl PolicyEngine {
    pub fn new(plan: &RuntimePlan, bias: f64) -> Self {
        Self {
            fans: plan.fans.clone(),
            filter: TemperatureFilter::configured(
                plan.channel_count,
                bias,
                plan.prediction.clone(),
            ),
            prediction: plan.prediction.clone(),
            bias,
            seen: vec![None; plan.channel_count],
            rising: vec![false; plan.channel_count],
            fresh_rising: false,
            captured: vec![None; plan.channel_count],
            required: plan
                .sensors
                .iter()
                .flat_map(|s| std::iter::repeat_n(!s.optional, s.channels.len()))
                .collect(),
            temperatures: vec![None; plan.channel_count],
            effective_temperatures: vec![None; plan.channel_count],
            decision: Decision {
                commands: Vec::with_capacity(plan.fans.len()),
                reason: DecisionReason::Curve,
                rising: false,
            },
        }
    }
    pub fn evaluate(
        &mut self,
        samples: &[Sample],
        now: Duration,
        sensor_fault: bool,
    ) -> Result<&Decision> {
        ensure!(
            samples.len() == self.temperatures.len(),
            "sample layout mismatch"
        );
        self.fresh_rising = false;
        let mut stale_required = false;
        for (i, sample) in samples.iter().enumerate() {
            self.temperatures[i] = sample.value(now);
            stale_required |= self.required[i]
                && matches!(
                    sample.availability(now),
                    Availability::Expired | Availability::Missing | Availability::ReadError
                );
            let key = (sample.sequence, sample.generation);
            if self.seen[i].is_some_and(|(_, generation)| generation != sample.generation)
                || self.captured[i].is_some_and(|last| {
                    sample.captured_at < last
                        || sample.captured_at.saturating_sub(last).as_secs_f64()
                            > self.prediction.max_sample_gap_seconds
                })
            {
                self.filter.reset_channel(i);
                for fan in &mut self.fans {
                    if fan.channels.contains(&i) {
                        fan.curve.interrupt_timing();
                    }
                }
            }
            if self.temperatures[i].is_none() {
                self.filter.reset_channel(i);
                self.effective_temperatures[i] = None;
                self.rising[i] = false;
            } else if self.seen[i] != Some(key) {
                let (effective, rising) =
                    self.filter
                        .update_channel(i, self.temperatures[i], sample.captured_at);
                self.effective_temperatures[i] = effective;
                self.rising[i] = rising;
                self.fresh_rising |= rising;
            }
            self.seen[i] = Some(key);
            self.captured[i] = Some(sample.captured_at);
        }
        let sensor_fault = sensor_fault || stale_required;
        if !sensor_fault && self.temperatures.iter().all(Option::is_none) {
            self.force_emergency(DecisionReason::SensorFault);
            anyhow::bail!("all temperature inputs are unavailable");
        }
        self.decision.rising = self.rising.iter().any(|r| *r);
        if sensor_fault {
            self.force_emergency(DecisionReason::SensorFault);
        } else {
            self.decision.reason = DecisionReason::Curve;
            self.decision.commands.clear();
            let maximum = self
                .effective_temperatures
                .iter()
                .flatten()
                .copied()
                .reduce(f64::max);
            for fan in &mut self.fans {
                let evidence_time = fan
                    .channels
                    .iter()
                    .filter(|&&i| self.temperatures[i].is_some())
                    .map(|&i| samples[i].captured_at)
                    .min()
                    .unwrap_or(now);
                fan.curve.select_observed(
                    &self.effective_temperatures,
                    maximum,
                    now,
                    evidence_time,
                );
                self.decision
                    .commands
                    .push(fan.commands[fan.curve.current_index()]);
            }
        }
        Ok(&self.decision)
    }
    pub(crate) fn cooling_pending(&self) -> bool {
        self.fans.iter().any(|fan| fan.curve.cooling_pending())
    }
    pub(crate) fn fresh_rising(&self) -> bool {
        self.fresh_rising
    }
    pub fn force_emergency(&mut self, reason: DecisionReason) {
        self.decision.reason = reason;
        self.decision.commands.clear();
        for fan in &mut self.fans {
            fan.curve.hold_high();
            self.decision.commands.push(fan.fault_command());
        }
    }
    pub fn reset(&mut self) {
        for fan in &mut self.fans {
            fan.curve.reset();
        }
        self.filter = TemperatureFilter::configured(
            self.temperatures.len(),
            self.bias,
            self.prediction.clone(),
        );
        self.seen.fill(None);
        self.captured.fill(None);
        self.rising.fill(false);
        self.fresh_rising = false;
    }
}
