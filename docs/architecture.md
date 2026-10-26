# Architecture

The project is a single Rust crate. Native SMART FFI lives in
`hardware/backends/smart.rs`; C sources are used only for test libraries.

```text
YAML -> Config validation -> RuntimePlan
                                |
SensorHub acquisition -> PolicyEngine -> FanManager
                              ^              |
                              +-- feedback --+
```

## Responsibilities

| Module | Responsibility |
| --- | --- |
| `config/` | YAML schema, loading, validation, and runtime-plan compilation |
| `policy/` | Sample availability, prediction, hysteresis, and cooling delays; no hardware I/O |
| `hardware/sensors/` | Sensor ownership, corrections, acquisition state, and error budgets |
| `hardware/fans/` | Fan ownership, writes, restoration, rediscovery, and reload commit |
| `hardware/backends/` | hwmon discovery and native library access |
| `hardware/workers.rs` | Bounded worker partitions and completion notifications |
| `runtime/` | Scheduling, signals, coordination, adaptive intervals, and diagnostic presentation |
| `logging.rs` | Binary-only console/journal subscriber setup |

`Config::compile(widths)` fixes channel layout, corrections, fan associations,
and typed commands without hardware I/O. ThinkPad inputs with unspecified width
need a preceding read-only probe. Public `control` and `drivers` aliases remain
for library compatibility.

`PolicyEngine::evaluate` accepts samples and explicit time, enabling
[hardware-free replay](development.md#policy-replay). `Controller::tick/tick_at`
are synchronous interfaces that still access hardware.

## Scheduling and blocking boundaries

The main loop owns policy and fan writes. `mio::Poll` waits for deadlines, worker
Waker notifications, and signal-hook-mio's self-pipe. Signal handlers do not touch
hardware or log; the loop handles stop, sleep, wake, reload, and diagnostic events.

There are six fixed workers: two fast readers (hwmon, ThinkPad, libsensors), one
NVML reader, one SMART reader, one startup/reload preparer, and one fan probe
worker. Capacity is not borrowed between partitions. libsensors also requires a
global mutex. Each sensor has at most one outstanding read; completion channels
are bounded by sensor count. Full partitions wait for capacity notifications
rather than accumulating tasks or retrying on a short timer.

Acquisition moves through `Idle`, `InFlight`, and `TimedOut`. Device instances move
between the loop and workers without concurrent access. Each libsensors entry
publishes its result before the next entry is read. Completion notifications may
coalesce; the loop drains available results.

Publication and timeout checking share a lock, held only around bookkeeping and
publication, never hardware access. A timely queued result is not rejected merely
because the event loop runs late. A true timeout counts once even when the read
later succeeds. Only a fresh timely acquisition clears that fault.

Blocking reads cannot be forcibly cancelled. A stuck call retains its worker; no
replacement threads are spawned. Exit does not join indefinitely blocked readers.
Workers cannot write fans. Fan writes and lightweight identity checks still run
in the main loop, so blocking writes are not isolated by the read workers.

Fan probes have a deadline of two base intervals and recheck identity before
commit. Release increments the probe generation to reject stale ownership claims.
Sleep/wake handling increments acquisition generations; old results return device
ownership but cannot update current samples. Reload uses separate result channels.

Sampling and depulse use absolute deadlines. EINTR, diagnostic events, and
spurious wakeups do not restart timers. Capacity notifications cannot unpause the
controller or end a depulse early. Adaptive recovery uses fresh acquisition time.

## Hardware lifecycle

- hwmon caches resolved paths and device/inode identities. Read failures, identity
  changes, reset, directory changes, and periodic discovery checks trigger rediscovery.
- libsensors entries in a batch share one session; chip/feature references do not outlive it.
- SMART caches the library and symbols, but opens the disk and checks sleep state on each read.
- NVML lazily initializes and reuses its context. Device handles are borrowed during a read;
  errors discard the context for later initialization.

Fan ownership states are `Released`, `Active`, and `Protective`. Initial modes
and values are captured before control. Replacement devices receive new snapshots;
old state is never restored onto a new inode.

Unchanged commands can skip redundant writes, but periodic maintenance reasserts
manual mode and refreshes watchdogs. New sample events do not postpone that
maintenance. Restoration, depulse, and rediscovery invalidate the write cache.
Execution failures interrupt cooling timing and request protection. A successful
write does not establish that physical RPM has reached its target.

## Reload

1. Parse and validate YAML, then prepare and probe it in the preparation partition.
   The old controller continues. Failure or timeout retains the old configuration.
   If preparation capacity is occupied, send HUP again later.
2. Finish an active depulse. A reload plan identifies retained, added, removed,
   and already released devices, then restores removed fans.
3. If restoration partially fails, request protection on all known old fans and reject reload.
4. Transfer retained ownership and snapshots, then commit the new controller.

Hardware operations cannot be atomically rolled back across devices. Added fans
are not changed before commit. Retained ownership prevents a program-selected
manual mode from being mistaken for the initial state.

## Faults and restoration

Optional failures leave inputs unavailable and retry when due. Required failures
within budget request fault targets; failures exceeding budget cause protective
handling and nonzero exit. Expired required data also requests protection. All
inputs unavailable is an error, subject to startup/wake recovery handling.

Fan failures do not prevent attempts on other fans. Protective operation bypasses
cooling delays and disables depulse. Fatal runtime faults preserve protective
state; normal exit, one-shot completion, and explicit sleep restore original state.
See [fault targets](configuration.md#fault-targets) for write fallbacks.

Current suspend integration is signal-driven. The process does not subscribe to
logind or detect suspend by comparing clocks. See [operations](operations.md#suspend-and-resume)
for the distinction between service and terminal execution.
