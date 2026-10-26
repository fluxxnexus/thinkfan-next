use std::{
    fs,
    os::unix::fs::symlink,
    path::{Path, PathBuf},
};
use tempfile::TempDir;
use thinkfan_next::{
    config::Config,
    control::{FanCommand, Speed, ThinkPadCommand},
    drivers::{ConfiguredSensor, FanDevice, FileFan, SensorDevice, resolve_hwmon},
    runtime::{Controller, Options, acquire_lock},
};

struct Fixture {
    root: TempDir,
}
impl Fixture {
    fn new() -> Self {
        let f = Self {
            root: tempfile::tempdir().unwrap(),
        };
        f.write("temp1_input", "40000");
        f.write("pwm1", "73");
        f.write("pwm1_enable", "2");
        f
    }
    fn path(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }
    fn write(&self, name: &str, value: &str) {
        let path = self.path(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, value).unwrap();
    }
    fn read(&self, name: &str) -> String {
        fs::read_to_string(self.path(name)).unwrap()
    }
    fn config(&self) -> Config {
        Config::parse(&self.text()).unwrap()
    }
    fn text(&self) -> String {
        format!(
            "sensors:\n  - hwmon: {}\nfans:\n  - hwmon: {}\nlevels:\n  - [0, 0, 50]\n  - [255, 45, 120]\n",
            self.path("temp1_input").display(),
            self.path("pwm1").display()
        )
    }
    fn controller(&self) -> Controller {
        Controller::prepare(self.config(), Options::default()).unwrap()
    }
}
#[test]
fn probing_and_checking_hardware_do_not_write() {
    let f = Fixture::new();
    let mut c = f.controller();
    c.check_hardware().unwrap();
    drop(c);
    assert_eq!(f.read("pwm1"), "73");
    assert_eq!(f.read("pwm1_enable"), "2");
}
#[test]
fn dry_run_never_touches_fans_including_cleanup() {
    let f = Fixture::new();
    let mut c = Controller::prepare(
        f.config(),
        Options {
            dry_run: true,
            ..Options::default()
        },
    )
    .unwrap();
    c.tick().unwrap();
    c.emergency_all();
    c.restore().unwrap();
    drop(c);
    assert_eq!(f.read("pwm1"), "73");
    assert_eq!(f.read("pwm1_enable"), "2");
}
#[test]
fn pwm_control_and_clean_shutdown_restore_automatic_mode() {
    let f = Fixture::new();
    let mut c = f.controller();
    c.tick().unwrap();
    assert_eq!(f.read("pwm1"), "0");
    assert_eq!(f.read("pwm1_enable"), "1");
    f.write("temp1_input", "80000");
    c.tick().unwrap();
    assert_eq!(f.read("pwm1"), "255");
    c.restore().unwrap();
    assert_eq!(f.read("pwm1_enable"), "2");
}
#[test]
fn manual_initial_duty_cycle_is_restored_too() {
    let f = Fixture::new();
    f.write("pwm1_enable", "1");
    {
        let mut c = f.controller();
        c.tick().unwrap();
        assert_eq!(f.read("pwm1"), "0");
    }
    assert_eq!(f.read("pwm1_enable"), "1");
    assert_eq!(f.read("pwm1"), "73");
}
#[test]
fn required_sensor_failure_requests_configured_maximum_and_preserves_it_on_drop() {
    let f = Fixture::new();
    let config = Config::parse(&f.text().replace("255", "180")).unwrap();
    let mut c = Controller::prepare(config, Options::default()).unwrap();
    c.tick().unwrap();
    fs::remove_file(f.path("temp1_input")).unwrap();
    assert!(c.tick().is_err());
    assert_eq!(f.read("pwm1"), "180");
    drop(c);
    assert_eq!(f.read("pwm1"), "180");
    assert_eq!(f.read("pwm1_enable"), "1");
}
#[test]
fn retry_budget_uses_configured_maximum_then_recovers() {
    let f = Fixture::new();
    let mut config = Config::parse(&f.text().replace("255", "180")).unwrap();
    config.sensors[0].max_errors = 2;
    let mut c = Controller::prepare(config, Options::default()).unwrap();
    c.tick().unwrap();
    f.write("temp1_input", "bad");
    c.tick().unwrap();
    assert_eq!(f.read("pwm1"), "180");
    c.tick().unwrap();
    assert_eq!(f.read("pwm1"), "180");
    f.write("temp1_input", "40000");
    c.tick().unwrap();
    assert_eq!(f.read("pwm1"), "0");
    f.write("temp1_input", "bad");
    c.tick().unwrap();
    c.tick().unwrap();
    assert!(c.tick().is_err());
}
#[test]
fn optional_sensor_recovers_without_changing_slots() {
    let f = Fixture::new();
    let mut config = f.config();
    let mut optional = config.sensors[0].clone();
    optional.hwmon = Some(f.path("temp2_input"));
    optional.optional = true;
    config.sensors.push(optional);
    let mut c = Controller::prepare(config, Options::default()).unwrap();
    c.tick().unwrap();
    assert_eq!(c.temperatures, vec![Some(40.0), None]);
    f.write("temp2_input", "90000");
    c.tick().unwrap();
    assert_eq!(c.temperatures, vec![Some(40.0), Some(90.0)]);
    assert_eq!(f.read("pwm1"), "255");
    fs::remove_file(f.path("temp2_input")).unwrap();
    c.tick().unwrap();
    assert_eq!(f.read("pwm1"), "0");
}
#[test]
fn all_optional_sensors_absent_is_a_failure() {
    let f = Fixture::new();
    let mut config = f.config();
    config.sensors[0].optional = true;
    let mut c = Controller::prepare(config, Options::default()).unwrap();
    fs::remove_file(f.path("temp1_input")).unwrap();
    assert!(c.tick().is_err());
    assert_eq!(f.read("pwm1"), "255");
}
#[test]
fn invalid_reload_keeps_active_config_and_original_state() {
    let f = Fixture::new();
    let mut c = f.controller();
    c.tick().unwrap();
    let mut config = f.config();
    config.sensors[0].hwmon = Some(f.path("missing"));
    assert!(c.reload(config).is_err());
    assert_eq!(f.read("pwm1_enable"), "1");
    c.tick().unwrap();
    assert_eq!(f.read("pwm1"), "0");
    c.restore().unwrap();
    assert_eq!(f.read("pwm1_enable"), "2");
}
#[test]
fn successful_reload_keeps_original_hardware_ownership() {
    let f = Fixture::new();
    let mut c = f.controller();
    c.tick().unwrap();
    let config = Config::parse(&f.text().replace("[0, 0, 50]", "[128, 0, 50]")).unwrap();
    c.reload(config).unwrap();
    c.tick().unwrap();
    assert_eq!(f.read("pwm1"), "128");
    drop(c);
    assert_eq!(f.read("pwm1_enable"), "2");
}
#[test]
fn reload_can_change_fan_topology() {
    let f = Fixture::new();
    f.write("pwm2", "30");
    f.write("pwm2_enable", "2");
    let mut c = f.controller();
    c.tick().unwrap();
    let mut config = f.config();
    config.fans[0].hwmon = Some(f.path("pwm2"));
    c.reload(config).unwrap();
    assert_eq!(f.read("pwm1_enable"), "2");
    c.tick().unwrap();
    assert_eq!(f.read("pwm2_enable"), "1");
    drop(c);
    assert_eq!(f.read("pwm2_enable"), "2");
}
#[test]
fn resolves_name_and_model_and_rejects_ambiguous_matches() {
    let f = Fixture::new();
    for i in 0..2 {
        f.write(&format!("hwmon{i}/name"), "nvme\n");
        f.write(&format!("hwmon{i}/temp1_input"), "45000");
    }
    f.write("hwmon0/device/model", "DISK-A\n");
    f.write("hwmon1/device/model", "DISK-B\n");
    assert!(resolve_hwmon(f.root.path(), Some("nvme"), None, Some(&[1]), false).is_err());
    let paths = resolve_hwmon(
        f.root.path(),
        Some("nvme"),
        Some("DISK-B"),
        Some(&[1]),
        false,
    )
    .unwrap();
    assert_eq!(paths, vec![f.path("hwmon1/temp1_input")]);
}
#[test]
fn follows_class_symlinks_and_deduplicates_same_interface() {
    let f = Fixture::new();
    f.write("device/hwmon0/name", "cpu");
    f.write("device/hwmon0/temp1_input", "40000");
    fs::create_dir(f.path("class")).unwrap();
    symlink(f.path("device/hwmon0"), f.path("class/hwmon0")).unwrap();
    symlink(f.path("device/hwmon0"), f.path("class/hwmon1")).unwrap();
    let result = resolve_hwmon(&f.path("class"), Some("cpu"), None, Some(&[1]), false).unwrap();
    assert_eq!(result, vec![f.path("device/hwmon0/temp1_input")]);
}
#[test]
fn rejects_duplicate_fan_paths_including_symlinks_before_writes() {
    let f = Fixture::new();
    symlink(f.path("pwm1"), f.path("alias")).unwrap();
    let mut config = f.config();
    let mut second = config.fans[0].clone();
    second.hwmon = Some(f.path("alias"));
    config.fans.push(second);
    config.levels = Config::parse(
        &f.text()
            .replace("[0, 0, 50]", "[[0, 0], 0, 50]")
            .replace("[255, 45, 120]", "[[255, 255], 45, 120]")
            .replace(
                "levels:",
                &format!("  - hwmon: {}\nlevels:", f.path("alias").display()),
            ),
    )
    .unwrap()
    .levels;
    assert!(Controller::prepare(config, Options::default()).is_err());
    assert_eq!(f.read("pwm1_enable"), "2");
}
#[test]
fn fractional_millidegrees_and_corrections_are_preserved() {
    let f = Fixture::new();
    f.write("temp1_input", "42125");
    let mut config = f.config().sensors.remove(0);
    config.correction = vec![-2.5];
    let mut sensor = ConfiguredSensor::new(config, false).unwrap();
    assert_eq!(sensor.read().unwrap(), vec![Some(39.625)]);
    for bad in ["NaN", "inf", "1000001", "-273151", "garbage"] {
        f.write("temp1_input", bad);
        assert!(sensor.read().is_err());
    }
}
#[test]
fn thinkpad_sensor_indices_are_one_based_and_preserve_requested_order() {
    let f = Fixture::new();
    f.write("thermal", "temperatures: 30 -128 60 40\n");
    let config = Config::parse(&f.text().replace(
        &format!("hwmon: {}", f.path("temp1_input").display()),
        &format!(
            "tpacpi: {}\n    indices: [3, 1, 2]",
            f.path("thermal").display()
        ),
    ))
    .unwrap();
    let mut sensor = ConfiguredSensor::new(config.sensors[0].clone(), false).unwrap();
    assert_eq!(sensor.read().unwrap(), vec![Some(60.0), Some(30.0), None]);
}
#[test]
fn thinkpad_auto_discovers_channels_and_rejects_changes() {
    let f = Fixture::new();
    f.write("thermal", "temperatures: 30 40\n");
    let mut config = f.config().sensors.remove(0);
    config.hwmon = None;
    config.tpacpi = Some(f.path("thermal"));
    let mut sensor = ConfiguredSensor::new(config, false).unwrap();
    assert_eq!(sensor.width(), 2);
    f.write("thermal", "temperatures: 30\n");
    assert!(sensor.read().is_err());
}
#[test]
fn missing_fan_sets_other_fans_to_configured_maximum() {
    let f = Fixture::new();
    f.write("pwm2", "0");
    f.write("pwm2_enable", "2");
    let text = f
        .text()
        .replace(
            "levels:",
            &format!("  - hwmon: {}\nlevels:", f.path("pwm2").display()),
        )
        .replace("[0, 0, 50]", "[[0, 0], 0, 50]")
        .replace("[255, 45, 120]", "[[180, 200], 45, 120]");
    let mut c = Controller::prepare(Config::parse(&text).unwrap(), Options::default()).unwrap();
    c.tick().unwrap();
    fs::remove_file(f.path("pwm1")).unwrap();
    assert!(c.tick().is_err());
    assert_eq!(f.read("pwm2"), "200");
    assert!(!f.path("pwm1").exists());
}
#[test]
fn activation_failure_still_restores_previous_mode() {
    let f = Fixture::new();
    let config = f.config().fans.remove(0);
    let mut fan = FileFan::probe(&config, 120, FanCommand::Pwm(255)).unwrap();
    fs::remove_file(f.path("pwm1")).unwrap();
    assert!(fan.apply(&Speed::Numeric(100)).is_err());
    fan.restore().unwrap();
    assert_eq!(f.read("pwm1_enable"), "2");
}
#[test]
fn instance_lock_cannot_be_acquired_twice_and_releases_on_drop() {
    let f = Fixture::new();
    let path = f.path("instance.lock");
    let lock = acquire_lock(&path).unwrap();
    assert!(acquire_lock(&path).is_err());
    drop(lock);
    assert!(acquire_lock(&path).is_ok());
}
#[test]
fn resume_reasserts_manual_control() {
    let f = Fixture::new();
    let mut c = f.controller();
    c.tick().unwrap();
    c.restore().unwrap();
    f.write("pwm1_enable", "2");
    c.reset();
    c.tick().unwrap();
    assert_eq!(f.read("pwm1_enable"), "1");
}
#[test]
fn optional_fan_can_appear_after_startup() {
    let f = Fixture::new();
    let mut config = f.config();
    config.fans[0].optional = true;
    fs::remove_file(f.path("pwm1")).unwrap();
    let mut c = Controller::prepare(config, Options::default()).unwrap();
    c.tick().unwrap();
    f.write("pwm1", "70");
    c.tick().unwrap();
    assert_eq!(f.read("pwm1"), "0");
    drop(c);
    assert_eq!(f.read("pwm1_enable"), "2");
}
#[test]
fn missing_hwmon_root_does_not_resolve_arbitrary_paths() {
    assert!(
        resolve_hwmon(
            Path::new("/nonexistent-test-hwmon"),
            Some("cpu"),
            None,
            Some(&[1]),
            false
        )
        .is_err()
    );
}

#[test]
fn fan_rediscovery_follows_name_after_hwmon_renumbering() {
    let f = Fixture::new();
    f.write("hwmon0/name", "testfan");
    f.write("hwmon0/pwm1", "73");
    f.write("hwmon0/pwm1_enable", "2");
    let mut config = f.config();
    config.fans[0].hwmon = Some(f.root.path().to_owned());
    config.fans[0].name = Some("testfan".into());
    config.fans[0].indices = Some(vec![1]);
    config.fans[0].optional = true;
    let mut c = Controller::prepare(config, Options::default()).unwrap();
    c.tick().unwrap();
    fs::remove_dir_all(f.path("hwmon0")).unwrap();
    c.tick().unwrap();
    f.write("hwmon4/name", "testfan");
    f.write("hwmon4/pwm1", "88");
    f.write("hwmon4/pwm1_enable", "2");
    c.tick().unwrap();
    assert_eq!(f.read("hwmon4/pwm1"), "0");
    drop(c);
    assert_eq!(f.read("hwmon4/pwm1_enable"), "2");
}
#[test]
fn replaced_inode_gets_its_own_original_state() {
    let f = Fixture::new();
    let mut c = f.controller();
    c.tick().unwrap();
    f.write("replacement", "99");
    fs::rename(f.path("replacement"), f.path("pwm1")).unwrap();
    f.write("pwm1_enable", "1");
    c.tick().unwrap();
    assert_eq!(f.read("pwm1"), "0");
    c.restore().unwrap();
    assert_eq!(f.read("pwm1"), "99");
    assert_eq!(f.read("pwm1_enable"), "1");
}
#[test]
fn old_fan_owner_does_not_restore_state_to_replaced_device() {
    let f = Fixture::new();
    let mut fan = FileFan::probe(&f.config().fans[0], 120, FanCommand::Pwm(200)).unwrap();
    fan.apply(&Speed::Numeric(120)).unwrap();
    f.write("replacement", "99");
    fs::rename(f.path("replacement"), f.path("pwm1")).unwrap();
    drop(fan);
    assert_eq!(f.read("pwm1"), "99");
    assert_eq!(f.read("pwm1_enable"), "1");
}
#[test]
fn emergency_attempts_pwm_even_if_mode_write_fails() {
    let f = Fixture::new();
    let mut fan = FileFan::probe(&f.config().fans[0], 120, FanCommand::Pwm(200)).unwrap();
    fan.apply(&Speed::Numeric(0)).unwrap();
    fs::remove_file(f.path("pwm1_enable")).unwrap();
    assert!(fan.emergency().is_err());
    assert_eq!(f.read("pwm1"), "200");
}
#[test]
fn malformed_thinkpad_interface_is_rejected_before_writes() {
    let f = Fixture::new();
    for contents in [
        "level: auto\n",
        "commands: level <level>\n",
        "level: unknown\ncommands: level <level>\n",
    ] {
        f.write("thinkpad", contents);
        let mut config = f.config().fans.remove(0);
        config.hwmon = None;
        config.tpacpi = Some(f.path("thinkpad"));
        assert!(
            FileFan::probe(
                &config,
                120,
                FanCommand::ThinkPad(ThinkPadCommand::Level(7))
            )
            .is_err()
        );
        assert_eq!(f.read("thinkpad"), contents);
    }
}
#[test]
fn thinkpad_speed_and_emergency_commands_use_driver_syntax() {
    let f = Fixture::new();
    f.write("thinkpad", "level: auto\ncommands: level <level>\n");
    let mut config = f.config().fans.remove(0);
    config.hwmon = None;
    config.tpacpi = Some(f.path("thinkpad"));
    let mut fan = FileFan::probe(
        &config,
        120,
        FanCommand::ThinkPad(ThinkPadCommand::Level(7)),
    )
    .unwrap();
    fan.apply(&Speed::Numeric(7)).unwrap();
    assert_eq!(f.read("thinkpad"), "level 7");
    fan.emergency().unwrap();
    assert_eq!(f.read("thinkpad"), "level 7");
}

#[test]
fn emergency_updates_decision_snapshot_and_resets_hysteresis() {
    let f = Fixture::new();
    let mut c = f.controller();
    c.tick().unwrap();
    assert_eq!(c.speeds, vec![Speed::Numeric(0)]);
    c.emergency_all();
    assert_eq!(c.speeds, vec![Speed::Numeric(255)]);
    f.write("temp1_input", "47000");
    c.tick().unwrap();
    assert_eq!(c.speeds, vec![Speed::Numeric(255)]);
}

fn seconds(n: u64) -> std::time::Duration {
    std::time::Duration::from_secs(n)
}
#[test]
fn controller_delays_downgrades_but_immediately_escalates_again() {
    let f = Fixture::new();
    let mut cfg = f.config();
    cfg.control.down_delay_seconds = 10.0;
    let mut c = Controller::prepare(cfg, Options::default()).unwrap();
    c.tick_at(seconds(0)).unwrap();
    f.write("temp1_input", "80000");
    c.tick_at(seconds(1)).unwrap();
    assert_eq!(f.read("pwm1"), "255");
    f.write("temp1_input", "40000");
    c.tick_at(seconds(2)).unwrap();
    c.tick_at(seconds(11)).unwrap();
    assert_eq!(f.read("pwm1"), "255");
    c.tick_at(seconds(12)).unwrap();
    assert_eq!(f.read("pwm1"), "0");
    f.write("temp1_input", "80000");
    c.tick_at(seconds(13)).unwrap();
    assert_eq!(f.read("pwm1"), "255");
}

#[test]
fn partial_reload_failure_reclaims_already_released_fans_immediately() {
    let f = Fixture::new();
    for n in 2..=3 {
        f.write(&format!("pwm{n}"), "73");
        f.write(&format!("pwm{n}_enable"), "2");
    }
    let mut cfg = f.config();
    let levels = cfg.levels.take();
    cfg.fans[0].levels = levels;
    for n in 2..=3 {
        let mut fan = cfg.fans[0].clone();
        fan.hwmon = Some(f.path(&format!("pwm{n}")));
        cfg.fans.push(fan);
    }
    let mut c = Controller::prepare(cfg.clone(), Options::default()).unwrap();
    c.tick().unwrap();
    let mut replacement = cfg;
    replacement.fans.drain(0..2);
    fs::remove_file(f.path("pwm2_enable")).unwrap();
    fs::create_dir(f.path("pwm2_enable")).unwrap();
    let error = c.reload(replacement).unwrap_err();
    assert!(format!("{error:#}").contains("partially failed"));
    assert_eq!(f.read("pwm1_enable"), "1");
    for n in 1..=3 {
        assert_eq!(f.read(&format!("pwm{n}")), "255");
    }
    assert_eq!(c.speeds, vec![Speed::Numeric(255); 3]);
}

#[test]
fn independent_sampling_reuses_only_unexpired_data_and_retries_on_due_time() {
    let f = Fixture::new();
    let mut cfg = f.config();
    cfg.sensors[0].interval_seconds = Some(1.0);
    cfg.sensors[0].max_age_seconds = Some(3.0);
    let mut slow = cfg.sensors[0].clone();
    slow.hwmon = Some(f.path("slow"));
    slow.interval_seconds = Some(10.0);
    slow.max_age_seconds = Some(20.0);
    slow.max_errors = 1;
    cfg.sensors.push(slow);
    f.write("slow", "45000");
    let mut c = Controller::prepare(cfg, Options::default()).unwrap();
    c.tick_at(seconds(0)).unwrap();
    f.write("slow", "bad");
    f.write("temp1_input", "55000");
    c.tick_at(seconds(1)).unwrap();
    assert_eq!(c.temperatures, vec![Some(55.0), Some(45.0)]);
    c.tick_at(seconds(10)).unwrap();
    assert_eq!(c.speeds, vec![Speed::Numeric(255)]);
    for n in 11..20 {
        c.tick_at(seconds(n)).unwrap();
    } // cached error must not exhaust budget
    assert!(c.tick_at(seconds(20)).is_err());
}
#[test]
fn cached_hwmon_detects_replacement_and_new_ambiguity() {
    let f = Fixture::new();
    f.write("hwmon/hwmon0/name", "cpu");
    f.write("hwmon/hwmon0/temp1_input", "40000");
    let mut cfg = f.config();
    cfg.sensors[0].hwmon = Some(f.path("hwmon"));
    cfg.sensors[0].name = Some("cpu".into());
    cfg.sensors[0].indices = Some(vec![1]);
    let mut sensor = ConfiguredSensor::new(cfg.sensors[0].clone(), false).unwrap();
    assert_eq!(sensor.read().unwrap(), vec![Some(40.0)]);
    let generation = sensor.generation();
    // Keep old inode alive, so replacement cannot accidentally reuse its number.
    fs::rename(f.path("hwmon/hwmon0"), f.path("old-hwmon")).unwrap();
    f.write("hwmon/hwmon0/name", "cpu");
    f.write("hwmon/hwmon0/temp1_input", "60000");
    assert_eq!(sensor.read().unwrap(), vec![Some(60.0)]);
    assert!(sensor.generation() > generation);
    f.write("hwmon/hwmon1/name", "cpu");
    f.write("hwmon/hwmon1/temp1_input", "45000");
    assert!(sensor.read().is_err());
}

#[test]
fn high_temperature_follows_curve_and_allows_reload_and_reset() {
    let f = Fixture::new();
    let config = Config::parse(&f.text().replace("[255, 45, 120]", "[128, 45, 120]")).unwrap();
    let mut c = Controller::prepare(config.clone(), Options::default()).unwrap();
    f.write("temp1_input", "150000");
    c.tick_at(seconds(0)).unwrap();
    assert_eq!(c.speeds, vec![Speed::Numeric(128)]);
    assert_eq!(f.read("pwm1"), "128");
    c.reload(config).unwrap();
    c.reset();
    c.tick_at(seconds(1)).unwrap();
    assert_eq!(f.read("pwm1"), "128");
    f.write("temp1_input", "40000");
    c.tick_at(seconds(2)).unwrap();
    assert_eq!(f.read("pwm1"), "0");
    // Fault handling uses the configured maximum speed.
    fs::remove_file(f.path("temp1_input")).unwrap();
    assert!(c.tick_at(seconds(3)).is_err());
    assert_eq!(f.read("pwm1"), "128");
}

#[test]
fn reload_updates_fault_target_and_drop_preserves_it() {
    let f = Fixture::new();
    let config = |max| {
        Config::parse(
            &f.text()
                .replace("[255, 45, 120]", &format!("[{max}, 45, 120]")),
        )
        .unwrap()
    };
    let mut c = Controller::prepare(config(200), Options::default()).unwrap();
    c.tick().unwrap();
    c.reload(config(160)).unwrap();
    fs::remove_file(f.path("temp1_input")).unwrap();
    assert!(c.tick().is_err());
    assert_eq!(c.speeds, vec![Speed::Numeric(160)]);
    assert_eq!(f.read("pwm1"), "160");
    drop(c);
    assert_eq!(f.read("pwm1"), "160");
    assert_eq!(f.read("pwm1_enable"), "1");
}
#[test]
fn dry_run_fault_reports_configured_maximum_without_writes() {
    let f = Fixture::new();
    let config = Config::parse(&f.text().replace("[255, 45, 120]", "[180, 45, 120]")).unwrap();
    let mut c = Controller::prepare(
        config,
        Options {
            dry_run: true,
            ..Options::default()
        },
    )
    .unwrap();
    c.emergency_all();
    assert_eq!(c.speeds, vec![Speed::Numeric(180)]);
    drop(c);
    assert_eq!(f.read("pwm1"), "73");
    assert_eq!(f.read("pwm1_enable"), "2");
}
#[test]
fn panic_cleanup_uses_configured_fault_command() {
    let f = Fixture::new();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut fan = FileFan::probe(&f.config().fans[0], 120, FanCommand::Pwm(170)).unwrap();
        fan.apply(&Speed::Numeric(0)).unwrap();
        panic!("injected failure");
    }));
    assert!(result.is_err());
    assert_eq!(f.read("pwm1"), "170");
}

#[test]
fn adaptive_sampling_recovers_by_time_and_resets_on_resume_and_reload() {
    use std::time::Duration;
    let f = Fixture::new();
    let mut c = f.controller();
    c.tick_at(Duration::ZERO).unwrap();
    f.write("temp1_input", "50000");
    c.tick_at(Duration::from_secs(1)).unwrap();
    assert_eq!(c.next_interval, Duration::from_secs(2));
    f.write("temp1_input", "40000");
    c.tick_at(Duration::from_secs(2)).unwrap();
    assert_eq!(c.next_interval, Duration::from_millis(2200));
    for _ in 0..10 {
        c.tick_at(Duration::from_secs(2)).unwrap();
    }
    assert_eq!(c.next_interval, Duration::from_millis(2200));
    c.tick_at(Duration::from_secs(7)).unwrap();
    assert_eq!(c.next_interval, Duration::from_millis(3200));
    c.reset();
    assert_eq!(c.next_interval, Duration::from_secs(5));
    c.tick_at(Duration::from_secs(100)).unwrap();
    f.write("temp1_input", "50000");
    c.tick_at(Duration::from_secs(101)).unwrap();
    assert_eq!(c.next_interval, Duration::from_secs(2));
    c.reload(f.config()).unwrap();
    assert_eq!(c.next_interval, Duration::from_secs(5));
    c.restore().unwrap();
}
