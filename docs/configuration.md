# Configuration reference

Only `sensors`, `fans`, `levels`, and `control` are accepted at the root. Unknown
fields, duplicate YAML keys, empty sensor/fan lists, and invalid curves are rejected.
Global `levels` and per-fan `levels` are mutually exclusive. Without global levels,
every fan must define its own curve.

## Sensors

Each entry selects exactly one backend.

| Key | Value | Additional selectors |
| --- | --- | --- |
| `hwmon` | Absolute temperature file or hwmon directory path | `indices`, `name`, `model` |
| `tpacpi` | Absolute thermal interface path, usually `/proc/acpi/ibm/thermal` | Optional `indices` |
| `chip` | Full libsensors chip name | Required `ids`, selecting feature labels |
| `nvml` | PCI bus ID, such as `'01:00.0'` or `'0000:01:00.0'` | None |
| `atasmart` | Absolute disk device path | `-d` skips sleeping disks |

Common fields:

| Field | Default | Meaning |
| --- | --- | --- |
| `correction` | Zero for every channel | Celsius offsets; provided arrays must match the channel count exactly |
| `optional` | `false` | Failed inputs become unavailable without exhausting a required-device error budget |
| `max_errors` | `0` | Number of consecutive failures tolerated for a required device; success resets the count |
| `interval_seconds` | Adaptive control interval | Independent interval after a completed acquisition, 0.1–86400 seconds |
| `max_age_seconds` | Twice the independent interval, or twice `-s` | Sample lifetime; an explicit value requires `interval_seconds` and must exceed it |
| `read_timeout_seconds` | Twice `-s` | Acquisition deadline, 0.1–86400 seconds; independent of sample lifetime |

All `indices` are one-based, unique, and expanded in configuration order.
Directory selectors require indices. Direct file paths must not include directory
selectors. `name` matches the trimmed `name` file; `model` matches the trimmed
`device/model` file. Discovery follows hwmon/sysfs links with bounded traversal.
Ambiguous matches are rejected: narrow the path or add selectors.

At most one `tpacpi` sensor entry is allowed. Without indices, startup must read
its interface to discover the channel count. Specify indices if an optional
ThinkPad interface may be absent at startup. Runtime channel-count changes fail
instead of shifting other inputs' curve positions.

hwmon millidegrees are converted to Celsius. ThinkPad `-128` and sleeping disks
skipped by `-d` are unavailable inputs, not zero-degree readings. Required-device
failures request the configured fault targets during retries. An acquisition that
has timed out but has not returned is not submitted again.

```yaml
sensors:
  - hwmon: /sys/class/hwmon
    name: coretemp
    indices: [1, 2]
    correction: [0, 1.5]
  - chip: thinkpad-isa-0000
    ids: [CPU, 'GPU 1']
  - nvml: '01:00.0'
    optional: true
  - atasmart: /dev/sda
    max_errors: 2
```

## Fans

Each entry selects `hwmon` or `tpacpi`. hwmon directory selectors work as for
sensors, but `indices` must contain exactly one output. Use separate entries for
multiple PWM outputs. At most one ThinkPad fan entry is allowed. Two entries
resolving to the same file are rejected, including symbolic-link aliases.

Both backends support `optional`, `max_errors`, per-fan `levels`, and
`down_delay_seconds` (overriding the global delay).

PWM speeds are integers from 0 to 255. ThinkPad speeds are integers from 0 to 7,
or `auto`, `disengaged`, and `full-speed`, optionally prefixed with `level `.
ThinkPad fan control requires the kernel module option `fan_control=1`;
see [module setup](operations.md#thinkpad-kernel-module).

## Curves

Simple curves use the maximum available corrected and predicted temperature:

```yaml
levels:
  - [0, 0, 50]
  - [128, 45, 70]
  - [255, 65, 120]
```

Detailed curves assign thresholds in expanded sensor-channel order:

```yaml
levels:
  - speed: 0
    upper_limit: [60, 40]
  - speed: 128
    lower_limit: [55, 35]
    upper_limit: [75, 50]
  - speed: 255
    lower_limit: [70, 45]
```

Any participating input reaching the current upper limit requests an upshift.
All available participating inputs must be strictly below their lower limits to
request a downshift. `_` skips a channel at that boundary. Unavailable optional
inputs keep their positions. The first lower limit and final upper limit may be
omitted; interior boundaries are required.

Threshold arrays must match the total input width. A curve cannot mix simple and
detailed syntax. Thresholds must be finite, lower limits must be below upper
limits, and adjacent levels must strictly overlap. Numeric thresholds and speeds
must not decrease. The final PWM level must be at least 128. Validation checks
structure, not whether thresholds are appropriate for your hardware.

Global curves controlling multiple fans require one speed per fan, in fan order.
A simple row can be `[[80, 100], 0, 50]`. One fan accepts a scalar or a one-element
array. Per-fan curves control only their owning fan.

Upshifts may cross multiple levels immediately. Without a delay, downshifts may
also cross multiple levels. Initial selection starts at the highest level and
moves down, choosing the higher level within an overlap.

## Prediction and delayed downshifts

```yaml
control:
  down_delay_seconds: 10
  prediction:
    rise_rate: 0.4
    rate_smoothing_seconds: 2
    bias_decay_seconds: 25
    max_bias: 30
    max_sample_gap_seconds: 120
```

| Setting | Default | Meaning |
| --- | --- | --- |
| `down_delay_seconds` | 0 | Continuous cooling evidence required before dropping one level; range 0–86400 seconds |
| `rise_rate` | 0.4 | Smoothed rise-rate threshold in Celsius per second |
| `rate_smoothing_seconds` | 2 | Exponential rise-rate smoothing time constant |
| `bias_decay_seconds` | 25 | Exponential bias decay time constant |
| `max_bias` | 30 | Maximum absolute temperature bias in Celsius |
| `max_sample_gap_seconds` | 120 | Gap after which prediction history and cooling timing are restarted |

Every prediction setting must be finite, positive, and no greater than 86400.
Individual settings may be overridden independently.

Rise rate uses actual acquisition completion times. Smoothing uses
`1 - exp(-dt / tau)`; the measured temperature itself is not low-pass filtered.
Residual positive or negative bias decays by `exp(-dt / tau)` toward zero.
With a rising temperature, `-b` supplies a prediction horizon of `b / 2` seconds.
The bias is clamped to `[-max_bias, max_bias]` and replaces the decayed residual
only when its magnitude is larger. `-b` accepts -10 to 30 and defaults to 0;
a negative value can lower the temperature used by the normal curve.

With a positive downshift delay, at least one participating input must be valid.
Fresh acquisition times must establish continuous cooling before one level is
dropped. The next level starts a new timer. Rising temperatures interrupt the
wait. Initial curve selection does not wait for the delay, but startup fault
handling can hold the highest level and subsequently require delayed cooling.

Missing inputs reset their prediction history. Nonpositive time deltas reset
prediction; backward time also interrupts cooling timing. Long gaps and device
identity changes restart timing. Explicit wake handling and successful reload
rebuild history. No independent overtemperature channel is implemented.

## Sampling and expiry

On a detected rise, the adaptive interval becomes `min(-s, 2 seconds)`. It then
recovers by one second per base interval of fresh acquisition time, up to `-s`.
For `-s 5`, recovering from 2 to 5 seconds requires 15 seconds of fresh evidence.
Cached values, maintenance events, and multiple completions at the same time do
not advance recovery. Independent sensor intervals are unaffected.

An independent interval runs from completion to the next acquisition attempt.
Explicit `max_age_seconds` must be finite, within 0.1–86400 seconds, and strictly
greater than that interval. A sample expires exactly at its lifetime boundary.
Expired required data requests fault targets; optional expired data is excluded.
All inputs unavailable causes protective handling and may terminate the process.

The event loop wakes for sampling, expiry, read deadlines, and fan maintenance.
Cached readings can participate in a curve but cannot be counted as new evidence
for prediction or cooling. A slow input participating in cooling should have an
appropriate `max_sample_gap_seconds`. Depulse is skipped if it would cross the
next sampling deadline.

Read timeout and expiry are separate: old data may expire while a read is still
within its deadline. Reaching the read deadline counts as a failure. A timeout
and its later completion consume only one error-budget entry. Late success is
discarded; only a subsequent timely success recovers the sensor. Publication and
timeout checking are synchronized to avoid rejecting timely queued results.
A result retains its original completion timestamp when delivered later.

Blocking backend calls cannot be forcibly cancelled. Timeout does not spawn a
replacement thread. These deadlines apply to asynchronous daemon acquisition;
synchronous `Controller::tick` and `--check-hardware` cannot preempt reads.
See [scheduling](architecture.md#scheduling-and-blocking-boundaries).

## Fault targets

Each fan's final curve command is its fault target. It is requested immediately,
without a downshift delay. ThinkPad final modes such as `auto`, `full-speed`, or
`disengaged` are used as configured. Reload updates these targets.

If a fault command cannot be written, PWM falls back to requesting
`pwm*_enable=0`, while ThinkPad falls back to `level auto`. Consequently, a
configured maximum is not a guaranteed speed cap when writes fail. Fatal faults
leave protective state in place; normal exit restores the original state.

## Validation commands

`--check` performs no hardware access. A ThinkPad thermal interface without
explicit indices can only have its width fully checked at startup or with
`--check-hardware`. Hardware checking reads interfaces and original fan modes,
but does not test writes. Optional devices may be absent. Dry-run also reads fan
interfaces to validate their identity, but never writes modes, PWM, or watchdogs.
