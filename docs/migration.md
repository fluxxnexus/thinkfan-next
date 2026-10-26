# Migrating from thinkfan

thinkfan-next implements upstream YAML functionality, but is not an exact
compatibility layer for the original CLI, permissive parsing, or daemon model.

| Area | Behavior |
| --- | --- |
| Backends | hwmon, ThinkPad, libsensors, NVML, and libatasmart remain supported |
| YAML curves | Simple/detailed thresholds and global/per-fan curves remain supported |
| Legacy configuration | Removed; rewrite old text configurations as YAML |
| Default path | Only `/etc/thinkfan.yaml`; no fallback to `/etc/thinkfan.conf` |
| Validation | Unknown fields, short correction arrays, ambiguous devices, and non-overlapping thresholds are rejected |
| CLI | `-D` is removed; `-n` is accepted but foreground operation is already the default |
| Daemonization | Use systemd or OpenRC; no internal fork-to-background mode |
| Logging | tracing events to console or native journal; old log parsers need updating |
| Required sensor failures | Request each fan's configured final command during retries instead of controlling from old temperatures |
| Sleeping SMART disks | `-d` leaves the temperature unavailable instead of reporting zero |
| Temperature precision | Fractional Celsius values and corrections are preserved |
| Sleep signals | PWR pauses control and must be paired with USR2 to resume |

Run `thinkfan-next --check -c PATH` on existing YAML. A file named `my.conf` can
still contain YAML when explicitly selected; its extension does not enable legacy
parsing. Configuration checks may require fixes to field names, array lengths,
threshold overlaps, or duplicate devices.

Stop the previous controller before enabling this one. Keep hardware-validated
curves rather than adopting example thresholds without testing. Installation does
not convert configuration, start control, or enable a service automatically.

## Control differences

- `-b` uses actual rise rates and time constants, not per-sample temperature deltas.
  Retune if necessary; adaptive sampling still detects rises when `-b 0` is used.
- Delayed downshifts are disabled by default; set `down_delay_seconds` explicitly.
- Read timeout and sample lifetime are independent. Default read timeout is twice `-s`.
- The final curve command is the fault target. Confirm it is appropriate for every fan.
- Independent overtemperature configuration is not supported. Remove obsolete `overheat` fields.

See [configuration](configuration.md) for exact defaults and
[operations](operations.md) for service installation, logging, and the current
limitations of manually launched processes across suspend/resume.
