use thinkfan_next::{config::Config, control::Speed};

fn simple(levels: &str) -> String {
    format!(
        "sensors:\n  - hwmon: /sys/fake/temp1_input\nfans:\n  - hwmon: /sys/fake/pwm1\nlevels:\n{levels}\n"
    )
}
fn parse(levels: &str) -> Config {
    Config::parse(&simple(levels)).unwrap()
}

#[test]
fn startup_is_conservative_and_hysteresis_boundaries_are_exact() {
    let c = parse("  - [0, 0, 50]\n  - [128, 45, 70]\n  - [255, 65, 120]");
    let mut curve = c.curves(Some(1)).unwrap().remove(0);
    assert_eq!(curve.select(&[Some(47.0)]), &Speed::Numeric(128));
    assert_eq!(curve.select(&[Some(45.0)]), &Speed::Numeric(128));
    assert_eq!(curve.select(&[Some(44.9)]), &Speed::Numeric(0));
    assert_eq!(curve.select(&[Some(49.9)]), &Speed::Numeric(0));
    assert_eq!(curve.select(&[Some(50.0)]), &Speed::Numeric(128));
    assert_eq!(curve.select(&[Some(90.0)]), &Speed::Numeric(255));
    assert_eq!(curve.select(&[Some(20.0)]), &Speed::Numeric(0));
    assert_eq!(curve.select(&[Some(500.0)]), &Speed::Numeric(255));
}
#[test]
fn maximum_is_recomputed_and_can_fall() {
    let c = parse("  - [0, 0, 50]\n  - [255, 45, 120]");
    let mut curve = c.curves(Some(2)).unwrap().remove(0);
    assert_eq!(
        curve.select(&[Some(80.0), Some(30.0)]),
        &Speed::Numeric(255)
    );
    assert_eq!(curve.select(&[Some(20.0), Some(30.0)]), &Speed::Numeric(0));
}
#[test]
fn detailed_any_hot_input_raises_all_cool_inputs_lower() {
    let text = "sensors:\n  - hwmon: /sys/fake\n    indices: [1, 2, 3]\nfans:\n  - hwmon: /sys/fake/pwm1\nlevels:\n  - speed: 0\n    upper_limit: [60, 40, '_']\n  - speed: 255\n    lower_limit: [55, 35, '_']\n";
    let mut curve = Config::parse(text)
        .unwrap()
        .curves(Some(3))
        .unwrap()
        .remove(0);
    assert_eq!(
        curve.select(&[Some(20.0), Some(20.0), Some(999.0)]),
        &Speed::Numeric(0)
    );
    assert_eq!(
        curve.select(&[Some(20.0), Some(40.0), None]),
        &Speed::Numeric(255)
    );
    assert_eq!(
        curve.select(&[Some(54.0), Some(35.0), None]),
        &Speed::Numeric(255)
    );
    assert_eq!(
        curve.select(&[Some(54.0), Some(34.0), None]),
        &Speed::Numeric(0)
    );
}
#[test]
fn detailed_optional_hole_keeps_sensor_positions() {
    let text = "sensors:\n  - hwmon: /sys/fake\n    indices: [1, 2]\nfans:\n  - hwmon: /sys/fake/pwm1\nlevels:\n  - speed: 0\n    upper_limit: [30, 80]\n  - speed: 255\n    lower_limit: [25, 75]\n";
    let mut curve = Config::parse(text)
        .unwrap()
        .curves(Some(2))
        .unwrap()
        .remove(0);
    assert_eq!(curve.select(&[None, Some(60.0)]), &Speed::Numeric(0));
    assert_eq!(curve.select(&[Some(40.0), None]), &Speed::Numeric(255));
}
#[test]
fn all_examples_validate_without_hardware() {
    let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
    let mut examples = std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "yaml"))
        .collect::<Vec<_>>();
    examples.sort();
    assert!(!examples.is_empty());
    for path in examples {
        Config::load(&path).unwrap_or_else(|error| panic!("{}: {error:#}", path.display()));
    }
}
#[test]
fn global_fan_speeds_are_mapped_in_order() {
    let config = Config::parse(include_str!("../examples/global-multi-fan.yaml")).unwrap();
    let mut curves = config.curves(Some(2)).unwrap();
    assert_eq!(
        curves[0].select(&[Some(30.0), Some(30.0)]),
        &Speed::Numeric(80)
    );
    assert_eq!(
        curves[1].select(&[Some(30.0), Some(30.0)]),
        &Speed::Numeric(90)
    );
}
#[test]
fn rejects_old_text_configuration_unconditionally() {
    for text in [
        "hwmon /sys/class/hwmon/hwmon0/temp1_input\n(0, 0, 50)\n(7, 45, 90)",
        "sensor /proc/acpi/ibm/thermal\n(0, 0, 50)",
        "/proc/acpi/ibm/thermal\n(0, 0, 50)",
    ] {
        assert!(Config::parse(text).is_err());
    }
}
#[test]
fn rejects_unknown_and_duplicate_keys() {
    let good = simple("  - [255, 0, 120]");
    for bad in [
        format!("{good}unknown: 1\n"),
        format!("{good}levels: []\n"),
        good.replace(
            "- hwmon: /sys/fake/temp1_input",
            "- hwmon: /sys/fake/temp1_input\n    optionl: true",
        ),
        good.replace(
            "- hwmon: /sys/fake/pwm1",
            "- hwmon: /sys/fake/pwm1\n    max_error: 2",
        ),
    ] {
        assert!(Config::parse(&bad).is_err(), "accepted: {bad}");
    }
}
#[test]
fn rejects_invalid_curves() {
    for levels in [
        "  - [256, 0, 120]",
        "  - [-1, 0, 120]",
        "  - [255, 20, 20]",
        "  - [255, .nan, 120]",
        "  - [255, 0, .inf]",
        "  - [0, 0, 50]\n  - [255, 50, 80]",
        "  - [0, 0, 50]\n  - [255, 55, 80]",
        "  - [255, 0, 50]\n  - [128, 45, 80]",
        "  - [0, 0, 50]\n  - [255, -1, 80]",
        "  - [127, 0, 120]",
        "  - ['level auto', 0, 120]",
        "  - [255, 0, 120, 99]",
        "  - speed: 255\n    upper_limit: [40, 60]",
        "  - speed: 255\n    upper_limit: ['ignore']",
        "  - speed: 0\n  - speed: 255\n    lower_limit: [30]",
    ] {
        assert!(
            Config::parse(&simple(levels)).is_err(),
            "accepted: {levels}"
        );
    }
}
#[test]
fn rejects_invalid_device_selectors() {
    let good = simple("  - [255, 0, 120]");
    for sensor in [
        "hwmon: relative",
        "hwmon: /fake\n    indices: []",
        "hwmon: /fake\n    indices: [0]",
        "hwmon: /fake\n    indices: [1, 1]",
        "hwmon: /fake\n    name: coretemp",
        "hwmon: /fake\n    ids: [CPU]",
        "hwmon: /fake\n    correction: [0, 1]",
        "hwmon: /fake\n    correction: [.nan]",
        "hwmon: /fake\n    nvml: '01:00.0'",
        "chip: coretemp",
        "chip: coretemp\n    ids: []",
        "nvml: '01:00.0'\n    indices: [1]",
    ] {
        let bad = good.replace("hwmon: /sys/fake/temp1_input", sensor);
        assert!(Config::parse(&bad).is_err(), "accepted: {sensor}");
    }
}
#[test]
fn global_and_local_levels_are_mutually_exclusive() {
    let text = simple("  - [255, 0, 120]").replace(
        "hwmon: /sys/fake/pwm1",
        "hwmon: /sys/fake/pwm1\n    levels: [[255, 0, 120]]",
    );
    assert!(Config::parse(&text).is_err());
}
#[test]
fn thinkpad_accepts_only_documented_commands() {
    for command in [
        "level auto",
        "level disengaged",
        "level full-speed",
        "level 7",
        "7",
        "auto",
    ] {
        assert!(Speed::Mode(command.into()).normalized(true).is_ok());
    }
    for command in [
        "level 8",
        "level 127",
        "level 3 junk",
        "auto\nwatchdog 0",
        "enable",
        "level -1",
    ] {
        assert!(Speed::Mode(command.into()).normalized(true).is_err());
    }
}
#[test]
fn yaml_aliases_comments_and_document_markers_remain_supported() {
    let text = "---\n# no hardware access needed\nsensors:\n  - hwmon: &device /sys/fake/device\nfans:\n  - hwmon: *device\nlevels:\n  - [0, 0, 50] # low speed\n  - [255, 45, 120]\n...\n";
    let config = Config::parse(text).unwrap();
    assert_eq!(config.sensors[0].hwmon, config.fans[0].hwmon);
    let mut curve = config.curves(Some(1)).unwrap().remove(0);
    assert_eq!(curve.select(&[Some(60.0)]), &Speed::Numeric(255));
}
#[test]
fn yaml_rejects_multiple_documents_and_nested_duplicate_keys() {
    let good = simple("  - [255, 0, 120]");
    for bad in [
        format!("{good}---\n{good}"),
        good.replace(
            "hwmon: /sys/fake/temp1_input",
            "hwmon: /sys/fake/temp1_input\n    optional: false\n    optional: true",
        ),
        simple("  - speed: 255\n    speed: 0"),
    ] {
        assert!(Config::parse(&bad).is_err(), "accepted {bad}");
    }
}
#[test]
fn yaml_preserves_fractional_thresholds_and_corrections() {
    let config = Config::parse(
        &simple("  - [0, 0.0, 50.25]\n  - [255, 45.125, 120.0]").replace(
            "hwmon: /sys/fake/temp1_input",
            "hwmon: /sys/fake/temp1_input\n    correction: [-2.5]",
        ),
    )
    .unwrap();
    assert_eq!(config.sensors[0].correction, vec![-2.5]);
    let mut curve = config.curves(Some(1)).unwrap().remove(0);
    assert_eq!(curve.select(&[Some(40.0)]), &Speed::Numeric(0));
    assert_eq!(curve.select(&[Some(50.24)]), &Speed::Numeric(0));
    assert_eq!(curve.select(&[Some(50.25)]), &Speed::Numeric(255));
    assert_eq!(curve.select(&[Some(45.125)]), &Speed::Numeric(255));
    assert_eq!(curve.select(&[Some(45.124)]), &Speed::Numeric(0));
}

#[test]
fn rejects_invalid_sampling_intervals_and_ttl() {
    let base = simple("  - [255, 0, 120]");
    for fields in [
        "interval_seconds: 0",
        "interval_seconds: .nan",
        "interval_seconds: 90000",
        "max_age_seconds: 10",
        "interval_seconds: 10\n    max_age_seconds: 10",
        "interval_seconds: 10\n    max_age_seconds: 9",
        "interval_seconds: 1\n    max_age_seconds: .inf",
    ] {
        let yaml = base.replace(
            "hwmon: /sys/fake/temp1_input",
            &format!("hwmon: /sys/fake/temp1_input\n    {fields}"),
        );
        assert!(Config::parse(&yaml).is_err(), "{yaml}");
    }
}

#[test]
fn removed_overheat_configuration_is_rejected() {
    for value in ["{critical: [90]}", "null", "{}"] {
        let yaml = simple("  - [255, 0, 120]").replace(
            "hwmon: /sys/fake/temp1_input",
            &format!("hwmon: /sys/fake/temp1_input\n    overheat: {value}"),
        );
        let error = Config::parse(&yaml).unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("unknown field") && message.contains("overheat"),
            "{message}"
        );
    }
}

#[test]
fn read_timeout_is_validated_independently_of_sampling_and_ttl() {
    let base = simple("  - [180, 0, 120]");
    for value in ["0", "-1", ".nan", ".inf", "86401"] {
        let yaml = base.replace(
            "hwmon: /sys/fake/temp1_input",
            &format!("hwmon: /sys/fake/temp1_input\n    read_timeout_seconds: {value}"),
        );
        assert!(Config::parse(&yaml).is_err());
    }
    let yaml = base.replace("hwmon: /sys/fake/temp1_input", "hwmon: /sys/fake/temp1_input\n    interval_seconds: 1\n    max_age_seconds: 2\n    read_timeout_seconds: 10");
    let plan = Config::parse(&yaml).unwrap().compile(&[1]).unwrap();
    assert_eq!(plan.sensors[0].read_timeout.unwrap().as_secs(), 10);
    assert_eq!(plan.sensors[0].max_age.unwrap().as_secs(), 2);
    assert!(
        Config::parse(&base.replace(
            "hwmon: /sys/fake/temp1_input",
            "hwmon: /sys/fake/temp1_input\n    read_timeout_seconds: 0.1"
        ))
        .is_ok()
    );
}
