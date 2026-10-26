# thinkfan-next

A Rust rewrite of [thinkfan](https://github.com/vmatare/thinkfan) for Linux,
implementing its YAML configuration and hardware interfaces. The implementation
was initially based on upstream commit `0e8649cbd39a99bd7a3e71aa81230488457fed18`.
It contains no C++ runtime and does not invoke `sensors`, `nvidia-smi`, or `smartctl`.

**YAML only.** The default configuration is `/etc/thinkfan.yaml`. Legacy text
configuration, format guessing, and fallback to `/etc/thinkfan.conf` are not supported.

## Features

- hwmon, ThinkPad thermal, libsensors, NVIDIA NVML, and ATA SMART temperature inputs.
- hwmon PWM and ThinkPad fan control, with independent or shared fan curves.
- Hysteresis, time-based temperature prediction, delayed downshifts, and independent sensor intervals.
- Bounded acquisition workers, sample expiry, device rediscovery, and configuration reload.
- Configured highest-level fault handling, state restoration, and ThinkPad watchdog support.
- Read-only checks, dry-run, console and native journal logging, systemd and OpenRC integration.

## Build

Requires Linux, Rust 1.89 or newer, a C compiler, libclang, and libsensors headers.

```sh
# Arch Linux
sudo pacman -S --needed base-devel rust clang lm_sensors

# Debian / Ubuntu (install Rust separately)
sudo apt-get install build-essential libclang-dev libsensors-dev

cargo build --release --locked
```

The executable is `target/release/thinkfan-next`. All builds link to
`libsensors.so.5` through `lm-sensors`; its transitive `sensors-sys` dependency
generates bindings at build time. NVML (`libnvidia-ml.so.1`) and libatasmart
(`libatasmart.so.4`) are loaded only when their backends are configured.

## Quick start

Choose an [example configuration](examples/thinkfan.yaml) and adapt it to your
hardware. Example curves are starting points, not validated thermal limits.
For ThinkPad fan control, first follow the [module setup instructions](docs/operations.md#thinkpad-kernel-module).

```sh
# Validate YAML and curves without accessing hardware.
./target/release/thinkfan-next --check -c examples/thinkfan.yaml

# Read hardware without writing fan state.
sudo ./target/release/thinkfan-next --check-hardware -c /etc/thinkfan.yaml

# Preview one decision without changing fans or watchdogs.
sudo ./target/release/thinkfan-next --dry-run --once -c /etc/thinkfan.yaml

# Run in the foreground.
sudo ./target/release/thinkfan-next -c /etc/thinkfan.yaml
```

Use `--help` or the [manual page](packaging/thinkfan-next.1) for command-line options.

## Documentation

| Guide | Contents |
| --- | --- |
| [Configuration](docs/configuration.md) | YAML fields, curves, prediction, sampling, and fault targets |
| [Operations](docs/operations.md) | Installation, ThinkPad setup, services, logging, and suspend/resume limits |
| [Migration](docs/migration.md) | Differences from upstream thinkfan |
| [Architecture](docs/architecture.md) | Module responsibilities, workers, ownership, and reload behavior |
| [Development](docs/development.md) | Tests, policy replay, performance measurement, and documentation conventions |
| [Arch Linux packaging](packaging/arch/README.md) | Local package builds and GitHub Actions artifacts |

The GitHub Actions package workflow builds an x86_64 Arch Linux package from the
checked-out source. It uploads a workflow artifact; it does not publish a GitHub
release or upload to the AUR.

## Validation and limitations

Automated tests cover simulated sysfs/procfs files, native library ABIs,
subprocesses, signals, blocked reads, and recovery. These checks do not certify
thermal behavior on a particular machine. Validate temperatures, RPM, and
recovery behavior on your hardware before relying on a configuration.

Fault handling requests each fan's final configured curve command. Failed writes
have backend-specific fallbacks; this is not an independent overtemperature
protection system. User-space software cannot guarantee recovery from SIGKILL,
a hung kernel, or hardware failure. Generic PWM interfaces have no universal
hardware watchdog.

Automatic logind sleep monitoring is **not implemented**. The supplied systemd
sleep helper signals the service process; manually launched processes need
separate sleep/resume integration. See [suspend and resume](docs/operations.md#suspend-and-resume).

## License

[GPL-3.0-or-later](LICENSE). See the [upstream project](https://github.com/vmatare/thinkfan)
for the original authors and contributors.
