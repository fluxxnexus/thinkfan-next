//! Entire decision replay without constructing a controller or touching hardware.
use std::time::Duration;
use thinkfan_next::{
    config::Config,
    control::FanCommand,
    policy::{
        DecisionReason, PolicyEngine,
        sample::{Availability, Sample, SampleState},
    },
};
fn t(n: u64) -> Duration {
    Duration::from_secs(n)
}
fn sample(value: f64, at: u64, sequence: u64) -> Sample {
    Sample {
        state: SampleState::Value(value),
        captured_at: t(at),
        valid_until: t(at + 100),
        sequence,
        generation: 1,
    }
}
fn engine(extra: &str, bias: f64) -> PolicyEngine {
    let yaml = format!(
        "sensors:\n  - hwmon: /not-present/cpu\n  - hwmon: /not-present/disk\nfans:\n  - hwmon: /not-present/fan\nlevels: [[0, 0, 50], [128, 45, 70], [255, 65, 120]]\n{extra}"
    );
    let plan = Config::parse(&yaml).unwrap().compile(&[1, 1]).unwrap();
    PolicyEngine::new(&plan, bias)
}
#[test]
fn repeated_cached_value_neither_repredicts_nor_advances_cooling() {
    let mut p = engine("control: {down_delay_seconds: 10}", 0.0);
    p.evaluate(&[sample(80.0, 0, 1), sample(40.0, 0, 1)], t(0), false)
        .unwrap();
    let cool = [sample(40.0, 1, 2), sample(40.0, 0, 1)];
    p.evaluate(&cool, t(1), false).unwrap();
    assert_eq!(
        p.evaluate(&cool, t(20), false).unwrap().commands,
        vec![FanCommand::Pwm(255)]
    );
    assert_eq!(
        p.evaluate(&[sample(40.0, 21, 3), sample(40.0, 21, 2)], t(21), false)
            .unwrap()
            .commands,
        vec![FanCommand::Pwm(128)]
    );
}
#[test]
fn prediction_consumes_each_acquisition_once_and_resets_on_new_device() {
    let mut p = engine("", 10.0);
    p.evaluate(&[sample(40.0, 0, 1), sample(30.0, 0, 1)], t(0), false)
        .unwrap();
    let mut samples = [sample(50.0, 5, 2), sample(30.0, 0, 1)];
    p.evaluate(&samples, t(5), false).unwrap();
    let effective = p.effective_temperatures.clone();
    p.evaluate(&samples, t(6), false).unwrap();
    assert_eq!(effective, p.effective_temperatures);
    samples[0] = sample(60.0, 7, 3);
    samples[0].generation = 2;
    p.evaluate(&samples, t(7), false).unwrap();
    assert_eq!(p.effective_temperatures[0], Some(60.0));
}
#[test]
fn expired_required_data_is_protective_without_new_failure_attempt() {
    let mut p = engine("", 0.0);
    let samples = [sample(40.0, 0, 1), sample(30.0, 0, 1)];
    assert_eq!(samples[0].availability(t(100)), Availability::Expired);
    assert_eq!(
        p.evaluate(&samples, t(100), false).unwrap().reason,
        DecisionReason::SensorFault
    );
    assert_eq!(p.decision.commands, vec![FanCommand::Pwm(255)]);
}
#[test]
fn runtime_plan_rejects_selector_mismatch_without_hardware() {
    let yaml = "sensors: [{hwmon: /missing, indices: [1, 2]}]\nfans: [{hwmon: /missing-pwm}]\nlevels: [[255, 0, 120]]";
    let config = Config::parse(yaml).unwrap();
    assert!(config.compile(&[1]).is_err());
    let plan = config.compile(&[2]).unwrap();
    assert_eq!(plan.sensors[0].channels, 0..2);
    assert_eq!(plan.fans[0].commands, vec![FanCommand::Pwm(255)]);
}

#[test]
fn execution_failure_holds_high_then_respects_down_delay() {
    let mut p = engine("control: {down_delay_seconds: 10}", 0.0);
    p.evaluate(&[sample(40.0, 0, 1), sample(30.0, 0, 1)], t(0), false)
        .unwrap();
    p.force_emergency(DecisionReason::FanFault);
    assert_eq!(p.decision.reason, DecisionReason::FanFault);
    assert_eq!(p.decision.commands, vec![FanCommand::Pwm(255)]);
    for at in [1, 10] {
        p.evaluate(
            &[sample(40.0, at, at + 1), sample(30.0, at, at + 1)],
            t(at),
            false,
        )
        .unwrap();
        assert_eq!(p.decision.reason, DecisionReason::Curve);
        assert_eq!(p.decision.commands, vec![FanCommand::Pwm(255)]);
    }
    p.evaluate(&[sample(40.0, 11, 12), sample(30.0, 11, 12)], t(11), false)
        .unwrap();
    assert_eq!(p.decision.commands, vec![FanCommand::Pwm(128)]);
}

#[test]
fn fault_targets_follow_each_fans_final_configured_level() {
    let yaml = "sensors: [{hwmon: /missing}]\nfans: [{hwmon: /pwm}, {tpacpi: /fan}]\nlevels: [[[0, 0], 0, 50], [[190, 7], 45, 120]]";
    let plan = Config::parse(yaml).unwrap().compile(&[1]).unwrap();
    let mut p = PolicyEngine::new(&plan, 0.0);
    for reason in [DecisionReason::SensorFault, DecisionReason::FanFault] {
        p.force_emergency(reason);
        assert_eq!(
            p.decision.commands,
            vec![
                FanCommand::Pwm(190),
                FanCommand::ThinkPad(thinkfan_next::control::ThinkPadCommand::Level(7))
            ]
        );
    }
}

#[test]
fn sample_availability_covers_time_boundaries_and_invalid_values() {
    let fresh = Sample {
        captured_at: t(10),
        valid_until: t(20),
        ..sample(42.0, 10, 1)
    };
    for (at, expected) in [
        (9, Availability::Expired),
        (10, Availability::Fresh),
        (19, Availability::Fresh),
        (20, Availability::Expired),
    ] {
        assert_eq!(fresh.availability(t(at)), expected, "at={at}");
        assert_eq!(
            fresh.value(t(at)),
            (expected == Availability::Fresh).then_some(42.0)
        );
    }
    for (state, expected) in [
        (SampleState::Value(f64::NAN), Availability::ReadError),
        (SampleState::Value(f64::INFINITY), Availability::ReadError),
        (
            SampleState::Value(f64::NEG_INFINITY),
            Availability::ReadError,
        ),
        (SampleState::Sleeping, Availability::Sleeping),
        (SampleState::Missing, Availability::Missing),
        (SampleState::ReadError, Availability::ReadError),
        (SampleState::Unavailable, Availability::Unavailable),
    ] {
        let input = Sample { state, ..fresh };
        assert_eq!(input.availability(t(10)), expected, "{state:?}");
        assert_eq!(input.value(t(10)), None);
    }
}
#[test]
fn expired_optional_input_does_not_fault_healthy_required_input() {
    let config = Config::parse("sensors: [{hwmon: /cpu}, {hwmon: /gpu, optional: true}]\nfans: [{hwmon: /pwm}]\nlevels: [[0, 0, 50], [180, 45, 120]]").unwrap();
    let mut policy = PolicyEngine::new(&config.compile(&[1, 1]).unwrap(), 0.0);
    let inputs = [sample(40.0, 100, 2), sample(90.0, 0, 1)];
    let decision = policy.evaluate(&inputs, t(100), false).unwrap();
    assert_eq!(decision.reason, DecisionReason::Curve);
    assert_eq!(decision.commands, [FanCommand::Pwm(0)]);
    assert_eq!(policy.temperatures, [Some(40.0), None]);
    assert!(policy.evaluate(&inputs, t(200), false).is_ok());
    assert_eq!(policy.decision.reason, DecisionReason::SensorFault);
    assert_eq!(policy.decision.commands, [FanCommand::Pwm(180)]);
}
