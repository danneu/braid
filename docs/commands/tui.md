---
experimental: false
---
[← braid](../index.md)

# braid tui

Interactive terminal dashboard showing pool state, disk health, allocation,
scrub status, and active alerts.

## When to use it

- Quick visual overview of your NAS health.
- Checking disk-level detail (LUKS cipher, SMART health, error counts, transport).
- Monitoring during or after a scrub.

## Basic example

```
sudo braid tui
```

## Demo mode

Try the TUI without a real pool (no config or btrfs required, no root required):

```
braid tui --demo
```

Demo mode shows three fake disks with sample data, useful for exploring the interface.

## Flags

| Flag | Effect |
| --- | --- |
| `--demo` | Run with fake data (no config, btrfs, or root required) |

## Keybindings

| Key | Action |
| --- | --- |
| `q` | Quit |
| `r` | Reload pool data now |
| `Tab` | Next tab |
| `Shift-Tab` | Previous tab |
| `j` / `k` | Select next/previous disk (Data/Scrub) or move within the focused Browse region |
| `h` / `l` | Move left/right across Browse regions |
| `Ctrl-D` / `Ctrl-U` | Page Browse content down/up (one screen at a time) |
| `Enter` | Open disk detail popup (Data) or drill into Browse content |
| `Esc` | Close disk detail popup or return from Browse drill-in |
| `?` | Toggle help overlay |
| `Shift-R` | Reset session temperature hi/lo watermarks |

## What it shows

**Main view** -- pool status, mount point, the `Profile` summary
(`data <X> | meta <Y> | system <Z>`, where each value is the profile name
verbatim for a single recognized profile such as `RAID1`, `DUP`, or `single`;
`partial` when that block-group type spans more than one profile; the raw
profile name verbatim for an unrecognized profile like `RAID5`; or `unknown`
only when no block groups of that type were reported), capacity bar, balance
state, and active alerts and advisories.

**Refreshing** -- while the pool is mounted, pool, disk, scrub, and alert data
refresh automatically about every 10 seconds and immediately when you press
`r`. While the pool is not mounted, that data stays manual-only via `r`. When
enabled, Fans and UPS telemetry also refresh automatically every 5 seconds and
immediately on `r`. The footer's `Reload: r` spinner and idle `(Xms)` duration
reflect pool refreshes, including automatic pool refreshes; automatic Fans/UPS
polls do not update it. The view redraws periodically while idle so relative
`ago` times stay current.

**Disk table** -- one row per disk: number, name, bus (sata/usb/nvme), SMART health, temperature, btrfs device-error count, and allocated (shown as percent used and allocated/size).

**Fans** (when fan control is enabled) -- Data-tab row with a `daemon:`
header annotation for `hddfancontrol-braid.service`: `active` is green,
`activating` and `inactive` are yellow, `failed` is red, and `unknown` is
gray. The annotation is not a column; the columns are PWM (raw/255 plus
percent), RPM, Driving (the hottest drive and its temperature), and Curve.
See the [fan control guide](../guides/fan-control.md#tui-fans-panel).

**UPS** (when UPS support is enabled) -- Data-tab row with the same
`daemon:` header annotation for the NUT daemon. The columns are Status
(color-coded flags), Battery, Runtime, and Load. See the
[UPS guide](../guides/ups.md#tui-ups-panel) for Status severity.

**Disk detail popup** (press Enter on a disk) -- disk name, LUKS lock status, cipher, key size, keyslot count, an allocations table (type/profile/size plus unallocated), the btrfs device-error breakdown (read/write/flush/corruption/generation), and a `SMART` section with the health verdict plus its supporting evidence rows (per-protocol: SATA reallocated/pending/uncorrectable, or NVMe critical-warning/media-errors/available-spare/percentage-used). A row for an out-of-spec attribute is colored red. Temperature is not repeated here -- it has its own column in the disk table.

**Tabs** -- three tabs, switched with Tab / Shift-Tab:

- **Data** (default) -- pool allocation breakdown, disk table, capacity bar,
  plus Fans and UPS rows when enabled.
- **Scrub** -- per-device scrub state, progress, and timing.
- **Browse** -- raw CLI output inspector across five tool families: Btrfs, NUT (UPS), Systemd, SMART (smartctl), and lsblk. Btrfs views include filesystem usage/show/df/commit-stats, device usage/stats, subvolumes with drill-in plus raw full/snapshot/deleted/default views, scrub status/limits, balance status, quota status/qgroups, and inspect-internal chunks. UPS views include status, raw variables, supported instant commands, connected clients, settable variables, and UPS discovery. Systemd views include unit status, show, braid units, failed units, timers, and mounts. SMART views include device scan, health, info, attributes, and self-test/error logs. lsblk views include tree, filesystems, disks, all-columns, and SCSI. `NUT > UPSes` can help find the correct `ups.name` before UPS support is enabled.

## Related commands

- [status](status.md) -- non-interactive pool health output
- [ups status](ups-status.md) -- non-interactive UPS state output
