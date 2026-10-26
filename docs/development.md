# Development

## Checks

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo build --release --locked
```

Tests use temporary files, subprocesses, FIFOs, and C shared-library fixtures.
They must not require privileged access to real fan hardware. CLI tests exercise
signals, blocked acquisition, logging, and reload. Native ABI tests check library
lifetimes, units, and result publication. Policy tests use explicit timestamps.
The Rust workflow checks stable Rust and the declared minimum supported version.
The [Arch workflow](../.github/workflows/archlinux.yml) builds a distributable package.

## Policy replay

```rust
use std::time::Duration;
use thinkfan_next::{config::Config, policy::{PolicyEngine, sample::{Sample, SampleState}}};

let config = Config::parse("sensors: [{hwmon: /unused/temp}]\nfans: [{hwmon: /unused/pwm}]\nlevels: [[0, 0, 50], [255, 45, 120]]")?;
let plan = config.compile(&[1])?;
let mut engine = PolicyEngine::new(&plan, 0.0);
let samples = [Sample {
    state: SampleState::Value(65.0),
    captured_at: Duration::from_secs(1),
    valid_until: Duration::from_secs(4),
    sequence: 1,
    generation: 1,
}];
let decision = engine.evaluate(&samples, Duration::from_secs(1), false)?;
assert_eq!(decision.commands.len(), 1);
# Ok::<(), anyhow::Error>(())
```

This example performs no hardware access. `tests/policy_replay.rs` covers cached
samples, expiry, generation changes, independent capture times, and protective
feedback. `Controller::tick_at` is an integration hook, not a hardware-free API.

## Performance measurement

Measure hardware access and scheduling latency before optimizing arithmetic.
Metrics record cumulative counts, total duration, and maximum duration for discovery,
reads, initialization, library loading, control cycles, dispatch/result delay,
timeouts, worker occupancy/saturation, and fan probes/writes. Counter-only events
have zero duration. SIGUSR1 reports worker capacities, current occupancy, completed
work, longest current tasks, and scheduling statistics.

TRACE enables detailed timing; DEBUG includes scheduling lateness. Logging adds
overhead, so keep settings fixed when comparing runs. Compute percentiles from
individual observations; cumulative maxima are not percentile estimates.

```sh
make bench
# Equivalent:
cargo test --release --test performance --locked -- --ignored --nocapture
```

The benchmark creates two simulated hwmon inputs and one fan, runs 1,000 synchronous
dry-run cycles, and verifies that the fan file is unchanged. It excludes compilation
and does not represent asynchronous daemon or real sysfs/NVML/SMART latency.
Record the source revision, hardware, build mode, and logging settings, and repeat
measurements. Thermal, acoustic, and power improvements require actual device tests.

## Documentation conventions

- Write maintained documentation, example comments, and packaging instructions in English.
- Use one descriptive H1 per Markdown file, sentence-case headings, and fenced examples.
- Keep the README as the entry point; put field definitions in the configuration reference
  and deployment/logging instructions in the operations guide.
- Use relative links for repository files and stable English heading anchors.
- Describe implemented behavior. Label proposals and known limitations explicitly.
- Update defaults, examples, the manual page, and package instructions with related changes.
- Do not commit generated packages, build directories, machine-specific logs, or credentials.
