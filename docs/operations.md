# Operations

## Installation

For an Arch Linux package, use the [packaging guide](../packaging/arch/README.md).
To install a source build under `/usr/local`:

```sh
make
sudo make install
sudo make install-systemd
sudo make install-modprobe
```

`install-modprobe` installs the ThinkPad module option described below. It is
optional for machines using only other fan backends. Installation does not create
`/etc/thinkfan.yaml` or start a service. Copy and adapt an example explicitly:

```sh
sudo install -m644 examples/thinkfan.yaml /etc/thinkfan.yaml
sudo thinkfan-next --check -c /etc/thinkfan.yaml
sudo thinkfan-next --check-hardware -c /etc/thinkfan.yaml
sudo thinkfan-next --dry-run --once -c /etc/thinkfan.yaml
```

The Makefile supports `DESTDIR` for staged installs and `PREFIX` for the executable
and documentation. `install-systemd` substitutes the selected executable prefix
into the installed main unit. Do not run two fan controllers simultaneously.

## ThinkPad kernel module

ThinkPad fan writes require the `thinkpad_acpi` module parameter:

```conf
options thinkpad_acpi fan_control=1
```

The supplied file is
[`packaging/modprobe.d/thinkfan-next.conf`](../packaging/modprobe.d/thinkfan-next.conf).
Source installation uses `make install-modprobe`; the Arch package includes it
at `/usr/lib/modprobe.d/thinkfan-next.conf`. Local overrides belong under
`/etc/modprobe.d/`. The file enables the driver interface when the module loads;
it does not start the daemon or select a fan level.

Reboot after installing the option. If the module is loaded from the initramfs,
rebuild the initramfs first so it contains the option. On an Arch installation
using mkinitcpio:

```sh
sudo mkinitcpio -P
```

Use your distribution's equivalent when using another initramfs generator. If the
driver is built into the kernel, use `thinkpad_acpi.fan_control=1` on the kernel
command line instead. Do not unload an active fan-control driver just to apply
this setting.

After reboot, check:

```sh
cat /sys/module/thinkpad_acpi/parameters/fan_control
cat /proc/acpi/ibm/fan
```

The parameter should report `Y` or `1`. Available fan controls depend on the model
and firmware; the option does not add support for unsupported hardware. See the
[Linux ThinkPad ACPI driver documentation](https://docs.kernel.org/admin-guide/laptops/thinkpad-acpi.html).

## Services

After validating your configuration:

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now thinkfan-next.service
```

The main unit uses native journal logging and conflicts with `thinkfan.service`
and `fancontrol.service`. Its `Also=` entry enables the sleep helper when the main
unit is enabled. The helper starts through `sleep.target` and signals the main
service before sleep and after wake.

An [OpenRC script](../packaging/openrc/thinkfan-next) is also supplied. It uses
`supervise-daemon`, defaults to `/usr/local/sbin/thinkfan-next`, and requires
separate sleep/resume integration.

## Suspend and resume

The current implementation responds to **signals**, not automatic logind events.

- `SIGPWR` restores fan state, invalidates acquisition history, and pauses control.
- `SIGUSR2` resets state and resumes acquisition, with a recovery window of four base intervals.
- Results belonging to an earlier acquisition generation cannot update current samples.

The supplied systemd sleep helper targets only `thinkfan-next.service`. A process
started manually in a terminal does not receive those notifications. Arrange
signals through your own session integration or run the installed service if you
need the supplied sleep integration. Do not signal every process by name when
multiple instances may exist.

Without sleep notifications, an NVML acquisition spanning suspend/resume may
exceed its deadline and report a transient failure even if the driver eventually
returns a temperature. Increasing the read deadline does not invalidate stale
pre-sleep data or implement suspend handling. Direct logind `PrepareForSleep`
monitoring and clock-based fallback detection are proposed work, not current
features.

## Logging

`--log-output` accepts:

| Mode | Destination |
| --- | --- |
| `auto` (default) | Native journal when `JOURNAL_STREAM` matches stderr's device and inode; otherwise console |
| `console` | Human-readable stderr |
| `journal` | Native structured events through `tracing-journald` |

Console output includes local time and a UTC offset such as `+11:00`. Module paths
are hidden by default, shown with `-v`, and included for DEBUG/TRACE events.
Level colors require a terminal on stderr and an unset `NO_COLOR` variable.
Check/help/version output remains on stdout.

Journal events use `MESSAGE`, `PRIORITY`, and `F_`-prefixed event fields. ERROR maps
to 3, WARN to 4, INFO to 6, and DEBUG/TRACE to 7. No duplicate console event is
emitted. Initialization failure falls back to stderr with a warning; later journal
send failures do not trigger automatic fallback.

```sh
journalctl -u thinkfan-next.service -p warning
journalctl -u thinkfan-next.service -o verbose
journalctl -u thinkfan-next.service F_BACKEND=nvml
```

### Reading a sample

```text
INFO fan control sample mode=dry-run (no fan writes) temperatures=s0/coretemp.temp1=72.00°C s1/gpu=66.00°C target=fan0:level=7 state=cooling-delay
```

`s0` and `s1` are zero-based YAML sensor indices. `temp1` is the selected hwmon
channel. `fan0:level=7` is a ThinkPad target level; `fan0:pwm=180/255` is a PWM
target. Neither is measured RPM. `predicted` lists only control temperatures that
differ from measured temperatures.

| State | Meaning |
| --- | --- |
| `waiting-for-sensors` | Initial acquisitions are not complete |
| `sensor-fault` / `fan-fault` | Protective handling for unavailable data or a fan fault |
| `cooling-delay` | At least one fan is waiting for sustained cooling evidence |
| `curve` | Normal curve decision |

Unavailable channels are labeled `pending`, `read-error`, `missing`, `expired`,
`sleeping`, or `unavailable`. Dry-run summaries coalesce completions to at most one
routine summary per base `-s` interval when new samples exist. State, health, and
target changes are reported immediately. Maintenance alone does not repeat old
samples. `-v` or DEBUG logging shows every decision; `SIGUSR1` requests a snapshot.
Logging does not change sampling or fan maintenance.

### Filtering and diagnostics

The default level is INFO; `-v` enables DEBUG and `-vv` TRACE. `-q` only suppresses
the startup announcement. `RUST_LOG` overrides defaults:

```sh
RUST_LOG=thinkfan_next=debug thinkfan-next -c /etc/thinkfan.yaml
RUST_LOG=warn,thinkfan_next::runtime::reactor=trace thinkfan-next -c /etc/thinkfan.yaml
```

Invalid directives produce a warning and use the CLI default. Dry-run and USR1
snapshots are INFO events and can be hidden by a filter. Timeout events include
backend, device, deadline, elapsed time since dispatch, and error budget. Late
completion records whether the underlying read succeeded; recovery is logged.

## Signals

| Signal | Action |
| --- | --- |
| INT / TERM | Restore initial fan state and exit |
| HUP | Validate and probe a replacement configuration; keep the old one on failure |
| USR1 | Log temperatures, targets, worker occupancy, and scheduling statistics |
| PWR | Release control and pause |
| USR2 | Reset history and resume |

The default instance lock is `/run/thinkfan-next.lock`. It protects only processes
using the same lock path, not unrelated applications writing the hardware.
