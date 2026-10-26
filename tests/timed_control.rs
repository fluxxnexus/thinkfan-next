use std::time::Duration;
use thinkfan_next::{
    config::Config,
    control::{Prediction, Speed, TemperatureFilter},
};
fn t(seconds: f64) -> Duration {
    Duration::from_secs_f64(seconds)
}
fn close(a: f64, b: f64) {
    assert!((a - b).abs() < 1e-8, "{a} != {b}");
}
fn config(extra: &str) -> String {
    format!(
        "sensors:\n  - hwmon: /fake/temp\nfans:\n  - hwmon: /fake/pwm\nlevels: [[0, 0, 50], [128, 45, 70], [255, 65, 120]]\n{extra}"
    )
}
fn curve() -> thinkfan_next::control::Curve {
    Config::parse(&config("control: {down_delay_seconds: 10}\n"))
        .unwrap()
        .curves(Some(1))
        .unwrap()
        .remove(0)
}
#[test]
fn linear_rise_is_independent_of_sampling_frequency() {
    fn replay(step: f64) -> f64 {
        let mut filter = TemperatureFilter::new(1, 10.0);
        let mut value = 0.0;
        for n in 0..=(10.0 / step) as usize {
            let now = n as f64 * step;
            value = filter.update_at(&[Some(40.0 + now)], t(now)).0[0].unwrap();
        }
        value
    }
    close(replay(0.1), replay(1.0));
    close(replay(1.0), replay(5.0));
}
#[test]
fn equal_temperature_change_over_different_times_has_different_predictions() {
    let mut fast = TemperatureFilter::new(1, 10.0);
    let mut slow = TemperatureFilter::new(1, 10.0);
    fast.update_at(&[Some(40.0)], t(0.0));
    slow.update_at(&[Some(40.0)], t(0.0));
    let a = fast.update_at(&[Some(42.0)], t(0.1));
    let b = slow.update_at(&[Some(42.0)], t(5.0));
    assert!(a.1);
    assert!(!b.1);
    assert!(a.0[0].unwrap() > b.0[0].unwrap());
}
#[test]
fn bias_decay_uses_elapsed_time_for_both_signs() {
    for bias in [-10.0, 10.0] {
        let replay = |step: f64| {
            let settings = Prediction {
                rise_rate: 2.0,
                ..Prediction::default()
            };
            let mut filter = TemperatureFilter::configured(1, bias, settings);
            filter.update_at(&[Some(40.0)], t(0.0));
            let start = filter.update_at(&[Some(50.0)], t(1.0)).0[0].unwrap() - 50.0;
            let mut end = 0.0;
            for n in 1..=(10.0 / step) as usize {
                end =
                    filter.update_at(&[Some(50.0)], t(1.0 + n as f64 * step)).0[0].unwrap() - 50.0;
            }
            close(end, start * (-10.0_f64 / 25.0).exp());
            assert!(start * end > 0.0);
            end
        };
        close(replay(0.1), replay(5.0));
    }
}
#[test]
fn prediction_is_bounded_and_resets_on_missing_time_reversal_or_long_gap() {
    let mut f = TemperatureFilter::new(1, 30.0);
    f.update_at(&[Some(0.0)], t(0.0));
    assert_eq!(f.update_at(&[Some(100.0)], t(0.1)).0, vec![Some(130.0)]);
    f.update_at(&[None], t(1.0));
    assert_eq!(f.update_at(&[Some(90.0)], t(2.0)).0, vec![Some(90.0)]);
    f.update_at(&[Some(100.0)], t(3.0));
    assert_eq!(f.update_at(&[Some(110.0)], t(3.0)).0, vec![Some(110.0)]);
    assert_eq!(f.update_at(&[Some(120.0)], t(1.0)).0, vec![Some(120.0)]);
    assert_eq!(f.update_at(&[Some(130.0)], t(200.0)).0, vec![Some(130.0)]);
}
#[test]
fn channels_have_independent_rise_and_missing_state() {
    let mut f = TemperatureFilter::new(2, 10.0);
    f.update_at(&[Some(40.0), Some(40.0)], t(0.0));
    let samples = f.update_at(&[Some(50.0), Some(40.0)], t(5.0)).0;
    assert!(samples[0].unwrap() > 50.0);
    assert_eq!(samples[1], Some(40.0));
    let samples = f.update_at(&[None, Some(50.0)], t(10.0)).0;
    assert_eq!(samples[0], None);
    assert!(samples[1].unwrap() > 50.0);
}
#[test]
fn downgrade_requires_continuous_cooling_and_only_one_level_per_delay() {
    let mut c = curve();
    assert_eq!(c.select_at(&[Some(80.0)], t(0.0)), &Speed::Numeric(255));
    assert_eq!(c.select_at(&[Some(20.0)], t(1.0)), &Speed::Numeric(255));
    assert_eq!(c.select_at(&[Some(65.0)], t(10.0)), &Speed::Numeric(255)); // equality cancels
    assert_eq!(c.select_at(&[Some(20.0)], t(11.0)), &Speed::Numeric(255));
    assert_eq!(c.select_at(&[Some(20.0)], t(20.9)), &Speed::Numeric(255));
    assert_eq!(c.select_at(&[Some(20.0)], t(21.0)), &Speed::Numeric(128));
    assert_eq!(c.select_at(&[Some(20.0)], t(22.0)), &Speed::Numeric(128));
    assert_eq!(c.select_at(&[Some(20.0)], t(32.0)), &Speed::Numeric(0));
    assert_eq!(c.select_at(&[Some(90.0)], t(32.1)), &Speed::Numeric(255));
}
#[test]
fn downgrade_timer_resets_after_stall_and_emergency() {
    let mut c = curve();
    c.select_at(&[Some(90.0)], t(0.0));
    c.select_at(&[Some(20.0)], t(1.0));
    assert_eq!(c.select_at(&[Some(20.0)], t(200.0)), &Speed::Numeric(255));
    c.hold_high();
    assert_eq!(c.select_at(&[Some(20.0)], t(210.0)), &Speed::Numeric(255));
    assert_eq!(c.select_at(&[Some(20.0)], t(220.0)), &Speed::Numeric(128));
    c.reset();
    assert_eq!(c.select_at(&[Some(20.0)], t(221.0)), &Speed::Numeric(0));
}
#[test]
fn validates_new_configuration_and_channel_counts() {
    let base = config("");
    for extra in [
        "control: {down_delay_seconds: -1}",
        "control: {down_delay_seconds: .nan}",
        "control: {down_delay_seconds: 1e100}",
        "control: {prediction: {rise_rate: 0}}",
        "control: {prediction: {bias_decay_seconds: .inf}}",
        "control: {prediction: {typo: 1}}",
    ] {
        assert!(
            Config::parse(&format!("{base}{extra}\n")).is_err(),
            "{extra}"
        );
    }
    assert!(
        Config::parse(&base.replace(
            "hwmon: /fake/pwm",
            "hwmon: /fake/pwm\n    down_delay_seconds: -1"
        ))
        .is_err()
    );
    let cfg = Config::parse(&base.replace(
        "hwmon: /fake/temp",
        "tpacpi: /fake/thermal\n    correction: [0, 1]",
    ))
    .unwrap();
    assert!(cfg.sensors[0].validate_width(1).is_err());
    cfg.sensors[0].validate_width(2).unwrap();
}
#[test]
fn per_fan_delay_overrides_global_delay() {
    let yaml = config("control: {down_delay_seconds: 10}").replace(
        "hwmon: /fake/pwm",
        "hwmon: /fake/pwm\n    down_delay_seconds: 0",
    );
    let mut c = Config::parse(&yaml)
        .unwrap()
        .curves(Some(1))
        .unwrap()
        .remove(0);
    c.select_at(&[Some(90.0)], t(0.0));
    assert_eq!(c.select_at(&[Some(20.0)], t(0.1)), &Speed::Numeric(0));
}
