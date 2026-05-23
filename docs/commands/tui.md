[← braid](../index.md)

# braid tui

Interactive terminal dashboard showing pool state, disk health, allocation, scrub status, and active alerts. Refreshes on demand.

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
| `r` | Reload pool data |
| `Tab` | Next tab |
| `Shift-Tab` | Previous tab |
| `j` / `k` | Select next/previous disk (Data/Scrub) or move within the focused Browse region |
| `h` / `l` | Move left/right across Browse regions |
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
only when no block groups of that type were reported), capacity bar, scrub
state, balance state, and active alerts.

**Disk table** -- one row per disk showing name, size, allocated, unallocated, transport (sata/usb/nvme), SMART health, and error counts.

**Disk detail popup** (press Enter on a disk) -- LUKS cipher, key size, keyslot count, device errors breakdown (read/write/flush/corruption/generation), and SMART health.

**Tabs** -- three tabs, switched with Tab / Shift-Tab:

- **Data** (default) -- pool allocation breakdown, disk table, capacity bar.
- **Scrub** -- per-device scrub state, progress, and timing.
- **Browse** -- raw CLI output inspector for Btrfs and UPS data. Btrfs views include filesystem usage/show/df/commit-stats, device usage/stats, subvolumes with drill-in plus raw full/snapshot/deleted/default views, scrub status/limits, balance status, quota status/qgroups, and inspect-internal chunks. UPS views include status, raw variables, supported instant commands, connected clients, settable variables, and UPS discovery. `NUT > UPSes` can help find the correct `ups.name` before UPS support is enabled.

## Related commands

- [status](status.md) -- non-interactive pool health output
- [ups status](ups-status.md) -- non-interactive UPS state output
