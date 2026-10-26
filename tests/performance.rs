use std::{
    fs,
    time::{Duration, Instant},
};
use thinkfan_next::{
    config::Config,
    metrics::{Operation, snapshot},
    runtime::{Controller, Options},
};
#[test]
#[ignore = "explicit benchmark: cargo test --release --test performance -- --ignored --nocapture"]
fn repeated_hwmon_sampling() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("hwmon0");
    fs::create_dir(&dir).unwrap();
    for (file, value) in [
        ("name", "fixture"),
        ("temp1_input", "40000"),
        ("temp2_input", "45000"),
        ("pwm1", "77"),
        ("pwm1_enable", "2"),
    ] {
        fs::write(dir.join(file), value).unwrap();
    }
    let yaml = format!(
        "sensors:\n  - hwmon: {0}\n    name: fixture\n    indices: [1]\n  - hwmon: {0}\n    name: fixture\n    indices: [2]\nfans:\n  - hwmon: {1}\nlevels: [[0, 0, 50], [255, 45, 120]]",
        root.path().display(),
        dir.join("pwm1").display()
    );
    let mut controller = Controller::prepare(
        Config::parse(&yaml).unwrap(),
        Options {
            dry_run: true,
            ..Options::default()
        },
    )
    .unwrap();
    let before = snapshot(Operation::HwmonDiscovery);
    let start = Instant::now();
    for n in 0..1000 {
        controller.tick_at(Duration::from_secs(n * 5)).unwrap();
    }
    eprintln!(
        "1000 cycles: {:?}; discovery calls: {}",
        start.elapsed(),
        snapshot(Operation::HwmonDiscovery).calls - before.calls
    );
    for operation in [
        Operation::HwmonDiscovery,
        Operation::SensorRead,
        Operation::Cycle,
    ] {
        eprintln!("{operation:?}: {:?}", snapshot(operation));
    }
    assert_eq!(fs::read_to_string(dir.join("pwm1")).unwrap(), "77");
    assert_eq!(fs::read_to_string(dir.join("pwm1_enable")).unwrap(), "2");
}
