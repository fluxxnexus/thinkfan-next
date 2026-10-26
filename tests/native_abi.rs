//! Fake shared libraries exercise the actual FFI paths without GPU/disk access.
mod support;
use std::{
    fs,
    process::{Command, Stdio},
};
use support::{CommandOutput, Process, wait_for};

fn fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("native.c");
    fs::write(&source, include_str!("fixtures/native.c")).unwrap();
    let library = root.path().join("native.so");
    let output = Command::new("cc")
        .args(["-shared", "-fPIC", "-Wall", "-Werror"])
        .arg(&source)
        .arg("-o")
        .arg(&library)
        .output_timeout();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    for name in ["libnvidia-ml.so.1", "libatasmart.so.4", "libsensors.so.5"] {
        fs::copy(&library, root.path().join(name)).unwrap();
    }
    fs::write(root.path().join("pwm"), "77").unwrap();
    fs::write(root.path().join("pwm_enable"), "2").unwrap();
    root
}

#[test]
fn native_backends_use_correct_abis_units_order_and_cleanup() {
    let root = fixture();
    let config = root.path().join("config.yaml");
    fs::write(&config, format!("sensors:\n  - nvml: '01:00.0'\n    correction: [1]\n  - atasmart: /dev/awake-test\n  - atasmart: /dev/asleep-test\n  - chip: test-chip\n    ids: [GPU, CPU]\nfans:\n  - hwmon: {}\nlevels:\n  - [255, 0, 120]\n", root.path().join("pwm").display())).unwrap();
    let log = root.path().join("calls");
    let output = Command::new(env!("CARGO_BIN_EXE_thinkfan-next"))
        .env_remove("RUST_LOG")
        .env("LD_LIBRARY_PATH", root.path())
        .env("THINKFAN_TEST_LOG", &log)
        .arg("-c")
        .arg(&config)
        .args(["--dry-run", "--once", "-d"])
        .output_timeout();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(
        stderr.contains("temperatures=s0/gpu=61.00°C s1/smart.awake-test=40.00°C s2/smart.asleep-test=sleeping s3/test-chip.GPU=55.50°C s3/test-chip.CPU=42.25°C"),
        "{stderr}"
    );
    let calls = fs::read_to_string(log).unwrap();
    assert_eq!(
        calls.matches("nvml init\n").count(),
        calls.matches("nvml shutdown\n").count()
    );
    assert_eq!(
        calls.matches("disk open\n").count(),
        calls.matches("disk free\n").count()
    );
    assert!(!calls.contains("sleeping disk read"));
    assert!(calls.contains("awake disk read"));
    assert_eq!(
        calls.matches("sensors init\n").count(),
        calls.matches("sensors cleanup\n").count()
    );
    assert_eq!(fs::read_to_string(root.path().join("pwm")).unwrap(), "77");
}

#[test]
fn nvml_context_is_reused_and_reinitialized_after_failure() {
    let root = fixture();
    let config = root.path().join("config.yaml");
    let log = root.path().join("calls");
    let failure = root.path().join("gpu-unavailable");
    fs::write(root.path().join("temp"), "40000").unwrap();
    fs::write(&config, format!("sensors:\n  - nvml: '0000:01:00.0'\n    optional: true\n  - hwmon: {}\nfans:\n  - hwmon: {}\nlevels: [[255, 0, 120]]\n", root.path().join("temp").display(), root.path().join("pwm").display())).unwrap();
    let mut process = Process(
        Command::new(env!("CARGO_BIN_EXE_thinkfan-next"))
            .env_remove("RUST_LOG")
            .env("LD_LIBRARY_PATH", root.path())
            .env("THINKFAN_TEST_LOG", &log)
            .env("THINKFAN_TEST_NVML_FAILURE", &failure)
            .arg("-c")
            .arg(&config)
            .args(["--dry-run", "-s", "0.1"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let calls = || fs::read_to_string(&log).unwrap_or_default();
    wait_for("native backend progress", || {
        calls().matches("nvml read success\n").count() >= 3
    });
    assert_eq!(calls().matches("nvml init\n").count(), 1);
    assert_eq!(calls().matches("nvml shutdown\n").count(), 0);
    fs::write(&failure, "fail").unwrap();
    wait_for("native backend progress", || {
        calls().contains("nvml read failure")
    });
    fs::remove_file(&failure).unwrap();
    let prior_successes = calls().matches("nvml read success\n").count();
    wait_for("native backend progress", || {
        calls().matches("nvml read success\n").count() >= prior_successes + 3
    });
    assert!(calls().matches("nvml init\n").count() >= 2);
    process.signal("TERM");
    assert!(process.wait().success());
    let calls = calls();
    assert_eq!(
        calls.matches("nvml init\n").count(),
        calls.matches("nvml shutdown\n").count()
    );
    assert_eq!(fs::read_to_string(root.path().join("pwm")).unwrap(), "77");
}
#[test]
fn lm_sensors_rejects_failures_and_cleans_up() {
    let root = fixture();
    let config = root.path().join("config.yaml");
    fs::write(&config, format!("sensors:\n  - chip: test-chip\n    ids: [CPU]\nfans:\n  - hwmon: {}\nlevels: [[255, 0, 120]]\n", root.path().join("pwm").display())).unwrap();
    for failure in ["init", "read", "duplicate", "non-temperature"] {
        let log = root.path().join(format!("calls-{failure}"));
        let output = Command::new(env!("CARGO_BIN_EXE_thinkfan-next"))
            .env_remove("RUST_LOG")
            .env("LD_LIBRARY_PATH", root.path())
            .env("THINKFAN_TEST_LOG", &log)
            .env("THINKFAN_TEST_LM_FAILURE", failure)
            .env("THINKFAN_TEST_LM_SLOW_CLEANUP", "1")
            .arg("-c")
            .arg(&config)
            .args(["--dry-run", "--once"])
            .output_timeout();
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        let expected = match failure {
            "init" => "libsensors configuration error",
            "read" => "reading libsensors feature",
            "duplicate" => "ambiguous libsensors label",
            _ => "not a temperature input",
        };
        assert!(stderr.contains(expected), "{stderr}");
        let calls = fs::read_to_string(log).unwrap();
        assert_eq!(calls.matches("sensors init\n").count(), 1);
        assert_eq!(
            calls.matches("sensors cleanup\n").count(),
            1,
            "{failure}: {calls}"
        );
        assert_eq!(fs::read_to_string(root.path().join("pwm")).unwrap(), "77");
    }
}

#[test]
fn libsensors_batches_entries_and_keeps_optional_failures_local() {
    let root = fixture();
    let config = root.path().join("config.yaml");
    let log = root.path().join("calls");
    fs::write(&config,format!("sensors:\n  - chip: test-chip\n    ids: [CPU]\n  - chip: test-chip\n    ids: [MISSING]\n    optional: true\n  - chip: test-chip\n    ids: [GPU]\nfans: [{{hwmon: {}}}]\nlevels: [[255, 0, 120]]",root.path().join("pwm").display())).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_thinkfan-next"))
        .env_remove("RUST_LOG")
        .env("LD_LIBRARY_PATH", root.path())
        .env("THINKFAN_TEST_LOG", &log)
        .env("THINKFAN_TEST_LM_SLOW_CLEANUP", "1")
        .arg("-c")
        .arg(&config)
        .args(["--dry-run", "--once"])
        .output_timeout();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(
        stderr.contains("temperatures=s0/test-chip.CPU=42.25°C s1/test-chip.MISSING=read-error s2/test-chip.GPU=55.50°C"),
        "{stderr}"
    );
    let calls = fs::read_to_string(log).unwrap();
    assert_eq!(calls.matches("sensors init\n").count(), 1);
    assert_eq!(calls.matches("sensors cleanup\n").count(), 1);
}
#[test]
fn smart_library_is_cached_while_disks_are_opened_per_sample() {
    let root = fixture();
    let config = root.path().join("config.yaml");
    let log = root.path().join("calls");
    let trace = root.path().join("trace");
    fs::write(&config,format!("sensors: [{{atasmart: /dev/awake-test}}]\nfans: [{{hwmon: {}}}]\nlevels: [[255, 0, 120]]",root.path().join("pwm").display())).unwrap();
    let mut p = Process(
        Command::new(env!("CARGO_BIN_EXE_thinkfan-next"))
            .env("RUST_LOG", "thinkfan_next=trace")
            .env("LD_LIBRARY_PATH", root.path())
            .env("THINKFAN_TEST_LOG", &log)
            .arg("-c")
            .arg(&config)
            .args(["--dry-run", "-s", "0.1"])
            .stdout(Stdio::null())
            .stderr(fs::File::create(&trace).unwrap())
            .spawn()
            .unwrap(),
    );
    wait_for("native backend progress", || {
        fs::read_to_string(&log)
            .unwrap_or_default()
            .matches("disk free\n")
            .count()
            >= 3
    });
    p.signal("TERM");
    assert!(p.wait().success());
    let calls = fs::read_to_string(log).unwrap();
    assert_eq!(
        calls.matches("disk open\n").count(),
        calls.matches("disk free\n").count()
    );
    let trace = fs::read_to_string(trace).unwrap();
    assert_eq!(trace.matches("operation=SmartLoad").count(), 1, "{trace}");
}

// A blocked C call must not withhold a different entry's completed sample.
fn assert_blocked_backend_isolated(sensors: &str, block_variable: &str) {
    let root = fixture();
    let fifo = root.path().join("blocked");
    let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let config = root.path().join("config.yaml");
    let log = root.path().join("calls");
    fs::write(
        &config,
        format!(
            "sensors:\n{sensors}fans: [{{hwmon: {}}}]\nlevels: [[100, 0, 70], [180, 65, 120]]\n",
            root.path().join("pwm").display()
        ),
    )
    .unwrap();
    let mut process = Process(
        Command::new(env!("CARGO_BIN_EXE_thinkfan-next"))
            .env_remove("RUST_LOG")
            .env("LD_LIBRARY_PATH", root.path())
            .env("THINKFAN_TEST_LOG", &log)
            .env(block_variable, &fifo)
            .arg("-c")
            .arg(&config)
            .arg("--lock-file")
            .arg(root.path().join("pid"))
            .args(["-s", "5"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait_for("native backend progress", || {
        fs::read_to_string(&log)
            .unwrap_or_default()
            .contains("blocked read entered")
    });
    wait_for("native backend progress", || {
        fs::read_to_string(root.path().join("pwm"))
            .unwrap_or_default()
            .trim()
            == "100"
    });
    assert!(process.0.try_wait().unwrap().is_none());
    process.signal("TERM");
    assert!(process.wait().success());
}
#[test]
fn libsensors_publishes_first_entry_while_second_is_blocked() {
    assert_blocked_backend_isolated(
        "  - chip: test-chip\n    ids: [CPU]\n  - chip: test-chip\n    ids: [GPU]\n    optional: true\n",
        "THINKFAN_TEST_LM_GPU_BLOCK",
    );
}
#[test]
fn blocked_smart_does_not_starve_nvml() {
    assert_blocked_backend_isolated(
        "  - atasmart: /dev/awake-test\n    optional: true\n  - nvml: '0000:01:00.0'\n",
        "THINKFAN_TEST_DISK_BLOCK",
    );
}
