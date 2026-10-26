mod support;
use std::{
    fs,
    path::PathBuf,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use support::{CommandOutput, Process, wait_for};
use tempfile::TempDir;

fn binary() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_thinkfan-next"));
    command.env_remove("RUST_LOG");
    command
}
struct Harness {
    root: TempDir,
    config: PathBuf,
}
impl Harness {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("temp"), "40000").unwrap();
        fs::write(root.path().join("pwm"), "73").unwrap();
        fs::write(root.path().join("pwm_enable"), "2").unwrap();
        let config = root.path().join("thinkfan.yaml");
        let h = Self { root, config };
        h.write_config(0);
        h
    }
    fn write_config(&self, low: u16) {
        fs::write(&self.config, format!("sensors:\n  - hwmon: {}\nfans:\n  - hwmon: {}\nlevels:\n  - [{low}, 0, 50]\n  - [255, 45, 120]\n", self.root.path().join("temp").display(), self.root.path().join("pwm").display())).unwrap();
    }
    fn command(&self) -> Command {
        let mut command = binary();
        command
            .arg("-c")
            .arg(&self.config)
            .args(["-s", "0.1", "--lock-file"])
            .arg(self.root.path().join("daemon.lock"));
        command
    }
    fn read(&self, file: &str) -> String {
        fs::read_to_string(self.root.path().join(file)).unwrap()
    }
    fn wait_for(&self, file: &str, expected: &str) {
        wait_for(&format!("{file} to equal {expected}"), || {
            self.read(file) == expected
        });
    }
    fn spawn(&self) -> Process {
        Process(
            self.command()
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        )
    }
}
#[test]
fn check_and_dry_run_are_read_only() {
    let h = Harness::new();
    for args in [
        vec!["--check"],
        vec!["--check-hardware"],
        vec!["--dry-run", "--once"],
    ] {
        let output = h.command().args(args).output_timeout();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(h.read("pwm"), "73");
        assert_eq!(h.read("pwm_enable"), "2");
    }
    assert!(!h.root.path().join("daemon.lock").exists());
}
#[test]
fn check_accepts_yaml_only_regardless_of_filename() {
    let h = Harness::new();
    let path = h.root.path().join("custom.conf");
    fs::copy(&h.config, &path).unwrap();
    assert!(
        binary()
            .arg("-c")
            .arg(&path)
            .arg("--check")
            .output_timeout()
            .status
            .success()
    );
    fs::write(
        &path,
        "hwmon /sys/fake/temp1_input\n(0, 0, 50)\n(7, 45, 90)",
    )
    .unwrap();
    let output = binary()
        .arg("-c")
        .arg(&path)
        .arg("--check")
        .output_timeout();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("YAML"));
}
#[test]
fn invalid_cli_values_fail_without_touching_hardware() {
    let h = Harness::new();
    for args in [
        vec!["--interval", "NaN"],
        vec!["--interval", "0"],
        vec!["--interval", "inf"],
        vec!["--depulse", "inf"],
        vec!["--bias", "31"],
        vec!["--watchdog", "0"],
        vec!["-D"],
    ] {
        assert!(!h.command().args(args).output_timeout().status.success());
        assert_eq!(h.read("pwm_enable"), "2");
    }
}
#[test]
fn term_and_int_restore_fan_state() {
    for signal in ["TERM", "INT"] {
        let h = Harness::new();
        let mut child = h.spawn();
        h.wait_for("pwm", "0");
        child.signal(signal);
        assert!(child.wait().success());
        assert_eq!(h.read("pwm_enable"), "2");
    }
}
#[test]
fn sighup_commits_valid_config_and_rejects_invalid_config() {
    let h = Harness::new();
    let mut child = logged_process(&h, "0.1", "0");
    h.wait_for("pwm", "0");
    h.write_config(128);
    child.signal("HUP");
    h.wait_for("pwm", "128");
    fs::write(&h.config, "invalid: config").unwrap();
    child.signal("HUP");
    wait_for_log(&h, "reload rejected");
    assert_eq!(h.read("pwm"), "128");
    assert!(child.0.try_wait().unwrap().is_none());
    child.signal("USR1");
    child.signal("TERM");
    assert!(child.wait().success());
    assert_eq!(h.read("pwm_enable"), "2");
}
#[test]
fn sleep_pauses_control_until_wakeup() {
    let h = Harness::new();
    let mut child = h.spawn();
    h.wait_for("pwm", "0");
    child.signal("PWR");
    h.wait_for("pwm_enable", "2");
    fs::write(h.root.path().join("temp"), "90000").unwrap();
    thread::sleep(Duration::from_millis(250));
    assert_eq!(h.read("pwm_enable"), "2");
    child.signal("USR2");
    h.wait_for("pwm", "255");
    assert_eq!(h.read("pwm_enable"), "1");
    child.signal("TERM");
    assert!(child.wait().success());
}
#[test]
fn second_process_cannot_take_control() {
    let h = Harness::new();
    let mut child = h.spawn();
    h.wait_for("pwm", "0");
    let output = h.command().arg("--once").output_timeout();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("already running"));
    child.signal("TERM");
    assert!(child.wait().success());
}
#[test]
fn fatal_sensor_error_exits_nonzero_and_retains_configured_maximum() {
    let h = Harness::new();
    fs::write(
        &h.config,
        fs::read_to_string(&h.config)
            .unwrap()
            .replace("[255, 45, 120]", "[180, 45, 120]"),
    )
    .unwrap();
    let mut child = h.spawn();
    h.wait_for("pwm", "0");
    fs::write(h.root.path().join("temp"), "not a temperature").unwrap();
    assert!(!child.wait().success());
    assert_eq!(h.read("pwm"), "180");
    assert_eq!(h.read("pwm_enable"), "1");
}

#[test]
fn tracing_keeps_stdout_clean_and_honors_verbosity_and_quiet() {
    let h = Harness::new();
    for (args, startup, sample) in [
        (vec!["--once"], true, false),
        (vec!["--once", "-v"], true, true),
        (vec!["--once", "-vv"], true, true),
        (vec!["--once", "-q"], false, false),
        (vec!["--dry-run", "--once", "-q"], false, true),
    ] {
        let output = h.command().args(args).output_timeout();
        assert!(output.status.success());
        assert!(output.stdout.is_empty());
        let log = String::from_utf8_lossy(&output.stderr);
        assert_eq!(log.contains("starting thinkfan-next"), startup, "{log}");
        assert_eq!(log.contains("temperatures="), sample, "{log}");
        assert!(!log.contains('\u{1b}'), "ANSI escape in noninteractive log");
        for sample in log
            .lines()
            .filter(|line| line.contains("fan control sample"))
        {
            assert!(
                !sample.contains("Some(") && !sample.contains("Numeric("),
                "{sample}"
            );
            assert!(sample.contains("target=fan0:pwm="), "{sample}");
            assert!(
                !sample.contains("predicted="),
                "unchanged prediction should be omitted: {sample}"
            );
        }
    }
}
#[test]
fn console_log_has_local_timestamp_and_omits_repeated_context() {
    let h = Harness::new();
    for (timezone, offset) in [
        ("UTC0", "+00:00"),
        ("EST5", "-05:00"),
        ("AEDT-11", "+11:00"),
        ("NPT-5:45", "+05:45"),
    ] {
        for detailed in [false, true] {
            let mut command = h.command();
            command.env("TZ", timezone).arg("--once");
            if detailed {
                command.arg("-v");
            }
            let output = command.output_timeout();
            assert!(output.status.success());
            let log = String::from_utf8(output.stderr).unwrap();
            let startup = log
                .lines()
                .find(|line| line.contains("starting thinkfan-next"))
                .unwrap();
            assert_eq!(&startup[4..5], "-");
            assert_eq!(&startup[10..11], " ");
            assert_eq!(&startup[20..26], offset, "{startup}");
            assert!(startup.contains("INFO"), "{startup}");
            assert_eq!(startup.contains("thinkfan_next:"), detailed, "{startup}");
            assert!(!log.contains("controller{"), "{log}");
            assert!(!log.contains('\u{1b}'), "{log}");
        }
    }
}
#[test]
fn redirected_logs_ignore_stale_journal_environment() {
    let h = Harness::new();
    for mode in ["auto", "console"] {
        let output = h
            .command()
            .env("JOURNAL_STREAM", "0:0")
            .args(["--once", "--log-output", mode])
            .output_timeout();
        assert!(output.status.success());
        let log = String::from_utf8(output.stderr).unwrap();
        assert!(log.contains("starting thinkfan-next"), "{log}");
        assert!(!log.contains("journal logging unavailable"), "{log}");
    }
}

#[test]
fn rust_log_filters_events_without_changing_control() {
    let h = Harness::new();
    let output = h
        .command()
        .env("RUST_LOG", "error")
        .args(["--once", "-v"])
        .output_timeout();
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    assert_eq!(h.read("pwm"), "0");
    assert_eq!(h.read("pwm_enable"), "2");
    let output = h
        .command()
        .env("RUST_LOG", "thinkfan_next::runtime=debug")
        .arg("--once")
        .output_timeout();
    assert!(output.status.success());
    let log = String::from_utf8_lossy(&output.stderr);
    assert!(
        log.contains("DEBUG") && log.contains("temperatures=s0/hwmon.temp=40.00°C"),
        "{log}"
    );
}
#[test]
fn malformed_rust_log_warns_and_falls_back() {
    let h = Harness::new();
    let output = h
        .command()
        .env("RUST_LOG", "thinkfan_next=not-a-level")
        .args(["--dry-run", "--once"])
        .output_timeout();
    assert!(output.status.success());
    let log = String::from_utf8_lossy(&output.stderr);
    assert!(log.contains("invalid RUST_LOG"), "{log}");
    assert!(log.contains("temperatures="), "{log}");
    assert_eq!(h.read("pwm"), "73");
}
#[test]
fn fatal_errors_retain_context_in_tracing_output() {
    let h = Harness::new();
    fs::write(&h.config, "invalid: configuration").unwrap();
    let output = h.command().arg("--once").output_timeout();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let log = String::from_utf8_lossy(&output.stderr);
    assert!(
        log.contains("ERROR")
            && log.contains("YAML configuration")
            && log.contains("unknown field"),
        "{log}"
    );
    assert_eq!(h.read("pwm_enable"), "2");
}

fn wait_for_log(h: &Harness, expected: &str) {
    wait_for(&format!("log to contain {expected}"), || {
        h.read("log").contains(expected)
    });
}
fn logged_process(h: &Harness, interval: &str, depulse: &str) -> Process {
    // Build independently to avoid duplicate clap interval arguments.
    Process(
        binary()
            .arg("-c")
            .arg(&h.config)
            .args(["-s", interval, "-p", depulse, "--lock-file"])
            .arg(h.root.path().join("daemon.lock"))
            .stdout(Stdio::null())
            .stderr(fs::File::create(h.root.path().join("log")).unwrap())
            .spawn()
            .unwrap(),
    )
}
#[test]
fn signals_interrupt_long_poll_and_repeated_readiness_is_drained() {
    let h = Harness::new();
    let mut child = logged_process(&h, "30", "0");
    h.wait_for("pwm", "0");
    fs::write(h.root.path().join("temp"), "90000").unwrap();
    for sequence in 1..=4 {
        child.signal("USR1");
        let deadline = Instant::now() + Duration::from_secs(2);
        while h.read("log").matches("temperature snapshot").count() < sequence {
            assert!(
                Instant::now() < deadline,
                "signal self-pipe did not wake poll"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
    // A dump does not force a sample before the 30-second deadline.
    assert_eq!(h.read("pwm"), "0");
    child.signal("HUP");
    h.wait_for("pwm", "255");
    child.signal("TERM");
    assert!(child.wait().success());
    assert_eq!(h.read("pwm_enable"), "2");
}
#[test]
fn repeated_diagnostics_do_not_postpone_sampling() {
    let h = Harness::new();
    let mut child = logged_process(&h, "0.2", "0");
    h.wait_for("pwm", "0");
    fs::write(h.root.path().join("temp"), "90000").unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while h.read("pwm") != "255" {
        assert!(
            Instant::now() < deadline,
            "signals starved the sampling deadline"
        );
        child.signal("USR1");
        thread::sleep(Duration::from_millis(20));
    }
    child.signal("TERM");
    assert!(child.wait().success());
}
#[test]
fn paused_reactor_accepts_reload_and_dump_without_resuming() {
    let h = Harness::new();
    let mut child = logged_process(&h, "30", "0");
    h.wait_for("pwm", "0");
    child.signal("PWR");
    h.wait_for("pwm_enable", "2");
    h.write_config(128);
    child.signal("HUP");
    wait_for_log(&h, "configuration reloaded");
    child.signal("USR1");
    wait_for_log(&h, "temperature snapshot");
    assert_eq!(h.read("pwm_enable"), "2");
    assert_eq!(h.read("pwm"), "0");
    child.signal("USR2");
    h.wait_for("pwm", "128");
    child.signal("PWR");
    h.wait_for("pwm_enable", "2");
    child.signal("TERM");
    assert!(child.wait().success());
}
fn thinkpad_harness() -> Harness {
    let h = Harness::new();
    fs::write(
        h.root.path().join("fan"),
        "level: auto\ncommands: level <level>\n",
    )
    .unwrap();
    fs::write(
        &h.config,
        format!(
            "sensors:\n  - hwmon: {}\nfans:\n  - tpacpi: {}\nlevels: [[0, 0, 50], [7, 45, 120]]\n",
            h.root.path().join("temp").display(),
            h.root.path().join("fan").display()
        ),
    )
    .unwrap();
    h
}
#[test]
fn depulse_is_interruptible_by_termination() {
    let h = thinkpad_harness();
    let mut child = logged_process(&h, "30", "10");
    h.wait_for("fan", "level disengaged");
    child.signal("TERM");
    assert!(child.wait().success());
    assert_eq!(h.read("fan"), "watchdog 0");
}
#[test]
fn sleep_during_depulse_releases_control_without_reapplying_speed() {
    let h = thinkpad_harness();
    let mut child = logged_process(&h, "30", "10");
    h.wait_for("fan", "level disengaged");
    child.signal("PWR");
    h.wait_for("fan", "watchdog 0");
    child.signal("USR1");
    wait_for_log(&h, "temperature snapshot");
    assert_eq!(h.read("fan"), "watchdog 0");
    child.signal("TERM");
    assert!(child.wait().success());
}
#[test]
fn diagnostic_does_not_end_depulse_early() {
    let h = thinkpad_harness();
    let mut child = logged_process(&h, "30", "1");
    h.wait_for("fan", "level disengaged");
    child.signal("USR1");
    wait_for_log(&h, "temperature snapshot");
    assert_eq!(h.read("fan"), "level disengaged");
    h.wait_for("fan", "level 0");
    child.signal("TERM");
    assert!(child.wait().success());
}

#[test]
fn independent_sensor_deadline_preempts_long_control_interval() {
    let h = Harness::new();
    let yaml = fs::read_to_string(&h.config).unwrap().replace(
        "fans:",
        "    interval_seconds: 0.1\n    max_age_seconds: 1\nfans:",
    );
    fs::write(&h.config, yaml).unwrap();
    let mut child = logged_process(&h, "30", "0");
    h.wait_for("pwm", "0");
    fs::write(h.root.path().join("temp"), "90000").unwrap();
    let until = Instant::now() + Duration::from_secs(2);
    while h.read("pwm") != "255" {
        assert!(Instant::now() < until, "independent deadline was ignored");
        thread::sleep(Duration::from_millis(10));
    }
    child.signal("TERM");
    assert!(child.wait().success());
}
#[test]
fn long_depulse_is_skipped_to_preserve_fast_sensor_deadline() {
    let h = thinkpad_harness();
    let yaml = fs::read_to_string(&h.config).unwrap().replace(
        "fans:",
        "    interval_seconds: 0.1\n    max_age_seconds: 1\nfans:",
    );
    fs::write(&h.config, yaml).unwrap();
    let mut child = logged_process(&h, "30", "10");
    h.wait_for("fan", "level 0");
    thread::sleep(Duration::from_millis(250));
    assert_eq!(h.read("fan"), "level 0");
    child.signal("TERM");
    assert!(child.wait().success());
}

// A FIFO holds an actual blocking read without adding test hooks to production.
fn blocked_sensor(h: &Harness, name: &str) -> fs::File {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let path = h.root.path().join(name);
    let c_path = CString::new(path.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap()
}
fn add_blocked_optional(h: &Harness, name: &str) {
    let yaml = fs::read_to_string(&h.config).unwrap().replace("fans:", &format!("  - tpacpi: {}\n    indices: [1]\n    optional: true\n    interval_seconds: 0.1\n    max_age_seconds: 2\nfans:", h.root.path().join(name).display()));
    fs::write(&h.config, yaml).unwrap();
}
#[test]
fn blocked_optional_read_does_not_delay_fast_sensor_or_signals() {
    let h = Harness::new();
    let _blocked = blocked_sensor(&h, "slow");
    add_blocked_optional(&h, "slow");
    let mut child = logged_process(&h, "0.1", "0");
    h.wait_for("pwm", "0");
    fs::write(h.root.path().join("temp"), "90000").unwrap();
    h.wait_for("pwm", "255");
    child.signal("USR1");
    wait_for_log(&h, "temperature snapshot");
    child.signal("PWR");
    h.wait_for("pwm_enable", "2");
    child.signal("TERM");
    assert!(child.wait().success());
}
#[test]
fn stuck_required_read_expires_and_uses_configured_fault_level() {
    let h = Harness::new();
    let _blocked = blocked_sensor(&h, "slow");
    fs::write(h.root.path().join("temp"), "temperatures: 40").unwrap();
    let yaml = fs::read_to_string(&h.config)
        .unwrap()
        .replace(
            &format!("hwmon: {}", h.root.path().join("temp").display()),
            &format!(
                "tpacpi: {}\n    indices: [1]",
                h.root.path().join("temp").display()
            ),
        )
        .replace(
            "fans:",
            "    interval_seconds: 0.1\n    max_age_seconds: 0.3\nfans:",
        )
        .replace("[255, 45, 120]", "[180, 45, 120]");
    fs::write(&h.config, yaml).unwrap();
    let mut child = logged_process(&h, "0.1", "0");
    h.wait_for("pwm", "0");
    fs::rename(h.root.path().join("slow"), h.root.path().join("temp")).unwrap();
    assert!(!child.wait().success());
    assert_eq!(h.read("pwm"), "180");
    assert_eq!(h.read("pwm_enable"), "1");
}
#[test]
fn blocked_reload_can_be_abandoned_without_blocking_control_or_stop() {
    let h = Harness::new();
    let _blocked = blocked_sensor(&h, "slow");
    let mut child = logged_process(&h, "0.1", "0");
    h.wait_for("pwm", "0");
    add_blocked_optional(&h, "slow");
    child.signal("HUP");
    wait_for_log(&h, "reload preparation timed out");
    fs::write(h.root.path().join("temp"), "90000").unwrap();
    h.wait_for("pwm", "255");
    child.signal("TERM");
    assert!(child.wait().success());
}
#[test]
fn repeated_blocked_reloads_never_spawn_replacement_workers() {
    let h = Harness::new();
    let _blocked = blocked_sensor(&h, "slow");
    let mut child = logged_process(&h, "0.1", "0");
    h.wait_for("pwm", "0");
    add_blocked_optional(&h, "slow");
    child.signal("HUP");
    wait_for_log(&h, "reload preparation timed out");
    for count in 1..=6 {
        child.signal("HUP");
        wait_for("each occupied-worker reload to be rejected", || {
            h.read("log").matches("reload deferred").count() >= count
        });
    }
    let tasks = fs::read_dir(format!("/proc/{}/task", child.0.id()))
        .unwrap()
        .count();
    assert_eq!(tasks, 7, "expected one loop and six partitioned workers");
    fs::write(h.root.path().join("temp"), "90000").unwrap();
    h.wait_for("pwm", "255");
    child.signal("TERM");
    assert!(child.wait().success());
}
#[test]
fn blocked_startup_probe_is_interruptible() {
    let h = Harness::new();
    let _blocked = blocked_sensor(&h, "thermal");
    let yaml = fs::read_to_string(&h.config).unwrap().replace(
        &format!("hwmon: {}", h.root.path().join("temp").display()),
        &format!("tpacpi: {}", h.root.path().join("thermal").display()),
    );
    fs::write(&h.config, yaml).unwrap();
    let mut child = logged_process(&h, "0.1", "0");
    wait_for_log(&h, "starting thinkfan-next");
    // Wait for signal registration rather than assuming startup speed.
    wait_for("startup TERM handler registration", || {
        fs::read_to_string(format!("/proc/{}/status", child.0.id()))
            .unwrap()
            .lines()
            .find_map(|line| line.strip_prefix("SigCgt:"))
            .and_then(|mask| u64::from_str_radix(mask.trim(), 16).ok())
            .is_some_and(|mask| mask & (1 << (libc::SIGTERM - 1)) != 0)
    });
    child.signal("TERM");
    assert!(child.wait().success());
    assert_eq!(h.read("pwm_enable"), "2");
}

#[test]
fn completion_from_before_sleep_cannot_reenter_policy() {
    use std::io::Write;
    let h = Harness::new();
    let mut blocked = blocked_sensor(&h, "slow");
    add_blocked_optional(&h, "slow");
    let mut child = logged_process(&h, "0.1", "0");
    h.wait_for("pwm", "0");
    child.signal("PWR");
    h.wait_for("pwm_enable", "2");
    fs::remove_file(h.root.path().join("slow")).unwrap();
    fs::write(h.root.path().join("slow"), "temperatures: 30").unwrap();
    blocked.write_all(b"temperatures: 99").unwrap();
    drop(blocked);
    thread::sleep(Duration::from_millis(100));
    assert_eq!(h.read("pwm_enable"), "2");
    assert_eq!(h.read("pwm"), "0");
    child.signal("USR2");
    h.wait_for("pwm_enable", "1");
    thread::sleep(Duration::from_millis(100));
    assert_eq!(h.read("pwm"), "0");
    child.signal("TERM");
    assert!(child.wait().success());
}

#[test]
fn old_configuration_completion_cannot_override_reloaded_curve() {
    use std::io::Write;
    let h = Harness::new();
    let mut blocked = blocked_sensor(&h, "slow");
    add_blocked_optional(&h, "slow");
    let mut child = logged_process(&h, "0.1", "0");
    h.wait_for("pwm", "0");
    h.write_config(128);
    child.signal("HUP");
    h.wait_for("pwm", "128");
    blocked.write_all(b"temperatures: 99").unwrap();
    drop(blocked);
    thread::sleep(Duration::from_millis(200));
    assert_eq!(h.read("pwm"), "128");
    child.signal("TERM");
    assert!(child.wait().success());
}

#[test]
fn optional_fan_probe_cannot_block_sensor_control_or_signals() {
    let h = Harness::new();
    let yaml = fs::read_to_string(&h.config)
        .unwrap()
        .replace(
            "levels:",
            &format!(
                "  - hwmon: {}\n    optional: true\nlevels:",
                h.root.path().join("pwm2").display()
            ),
        )
        .replace("[0, 0, 50]", "[[0, 0], 0, 50]")
        .replace("[255, 45, 120]", "[[180, 200], 45, 120]");
    fs::write(&h.config, yaml).unwrap();
    let mut child = logged_process(&h, "0.1", "0");
    h.wait_for("pwm", "0");
    let _blocked = blocked_sensor(&h, "pwm2_enable");
    fs::write(h.root.path().join("pwm2"), "73").unwrap();
    thread::sleep(Duration::from_millis(150));
    fs::write(h.root.path().join("temp"), "90000").unwrap();
    h.wait_for("pwm", "180");
    child.signal("USR1");
    wait_for_log(&h, "worker occupancy");
    child.signal("TERM");
    assert!(child.wait().success());
    assert_eq!(h.read("pwm_enable"), "2");
}
#[test]
fn ttl_expiry_does_not_end_a_read_before_its_separate_timeout() {
    let h = Harness::new();
    let _blocked = blocked_sensor(&h, "slow");
    fs::write(h.root.path().join("temp"), "temperatures: 40").unwrap();
    let yaml = fs::read_to_string(&h.config).unwrap()
        .replace(&format!("hwmon: {}", h.root.path().join("temp").display()), &format!("tpacpi: {}\n    indices: [1]\n    interval_seconds: 0.1\n    max_age_seconds: 0.2\n    read_timeout_seconds: 1.5", h.root.path().join("temp").display()))
        .replace("[255, 45, 120]", "[180, 45, 120]");
    fs::write(&h.config, yaml).unwrap();
    let mut child = logged_process(&h, "0.1", "0");
    h.wait_for("pwm", "0");
    fs::rename(h.root.path().join("slow"), h.root.path().join("temp")).unwrap();
    h.wait_for("pwm", "180");
    thread::sleep(Duration::from_millis(200));
    assert!(child.0.try_wait().unwrap().is_none());
    assert!(!h.read("log").contains("acquisition timed out"));
    assert!(!child.wait().success());
    assert!(h.read("log").contains("acquisition timed out"));
}
