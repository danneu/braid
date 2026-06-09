# braid

braid is a NixOS CLI tool for managing an encrypted btrfs RAID1 NAS. These docs cover end-user workflows, command reference, design decisions, internals, and development practices.

## Common tasks

- **First time setup** -- [Getting started](guides/getting-started.md)
- **Add, remove, or replace a disk** -- [add](commands/add.md), [remove](commands/remove.md), [replace](commands/replace.md)
- **USB auto-unlock** -- [Auto-unlock](guides/auto-unlock.md)
- **Set up disk health alerts** -- [Monitoring and alerts](guides/monitoring-and-alerts.md)
- **Suspend when idle, wake on demand** -- [Power management](guides/power-management.md)

## Guides

| Guide                                                        | Description                                            |
| ------------------------------------------------------------ | ------------------------------------------------------ |
| [Install NixOS](guides/install-nixos.md)                     | Install NixOS itself before setting up braid           |
| [Getting started](guides/getting-started.md)                 | First-time setup: find disks, create pool, unlock      |
| [Day-to-day NAS usage](guides/day-to-day-nas-usage.md)       | Subvolumes, file permissions, Samba shares             |
| [Auto-unlock](guides/auto-unlock.md)                         | USB keyfile setup for unattended reboots               |
| [Monitoring and alerts](guides/monitoring-and-alerts.md)     | Disk health alerts, beeper, alert commands             |
| [Power management](guides/power-management.md)               | Auto-suspend, Wake-on-LAN, RTC wakeups                 |
| [Fan control](guides/fan-control.md)                         | HDD-driven chassis fan control, SATA hotswap           |
| [UPS](guides/ups.md)                                         | NUT-backed orderly poweroff, preflight safety, live status |
| [NixOS configuration](guides/nixos-configuration.md)         | Module options, scrub scheduling, pinned toolchain     |
| [Sharing and permissions](guides/sharing-and-permissions.md) | Storage group, mount permissions, Samba                |
| [Mounting subvolumes](guides/mounting-subvolumes.md)         | Expose a btrfs subvolume at a custom path              |
| [Troubleshooting](guides/troubleshooting.md)                 | ENOSPC balance, paused balance, missing devices        |
| [Recovery scenarios](guides/recovery-scenarios.md)           | Interrupted operations, lost pool.json, degraded mount |

## Commands

Commands marked 🧪 are experimental: the idea or implementation is still uncertain and may be removed, replaced, or overhauled before braid v1.0.

| Command                                         | Description                                          |
| ----------------------------------------------- | ---------------------------------------------------- |
| [add](commands/add.md)                          | Add disks to the pool or create a new pool           |
| [remove](commands/remove.md)                    | Remove a live disk from the pool                     |
| [remove-missing](commands/remove-missing.md)    | Forget a dead or missing device entry                |
| [replace](commands/replace.md)                  | Replace a live or dead disk                          |
| [unlock](commands/unlock.md)                    | Open LUKS devices and mount the pool                 |
| [lock](commands/lock.md)                        | Unmount the pool and close LUKS devices              |
| [seal-mountpoint 🧪](commands/seal-mountpoint.md) | Seal the offline mountpoint immutable (boot-managed) |
| [idle 🧪](commands/idle.md)                     | Check if the pool is idle for auto-suspend           |
| [status](commands/status.md)                    | Pool health, disk status, allocation, scrub info     |
| [doctor](commands/doctor.md)                    | Diagnostic checks for config and pool health         |
| [monitor 🧪](commands/monitor.md)               | Health check for alerting used by systemd timer      |
| [ack 🧪](commands/ack.md)                       | Acknowledge and silence an active alert              |
| [enroll 🧪](commands/enroll.md)                 | Enroll a USB keyfile for auto-unlock                 |
| [discover 🧪](commands/discover.md)             | Scan for braid LUKS devices and rebuild pool.json    |
| [recover 🧪](commands/recover.md)               | Recover from an interrupted operation                |
| [tui](commands/tui.md)                          | Interactive dashboard with raw-output Browse tab     |
| [ups status 🧪](commands/ups-status.md)         | Live UPS state from NUT, with JSON for scripts       |

## Design

| Doc                                                     | Purpose                                      |
| ------------------------------------------------------- | -------------------------------------------- |
| [Principles](design/principles.md)                      | Authoritative invariants for braid behavior  |
| [Decision records](design/decisions/001-btrfs-raid1.md) | Rationale, history, and rejected alternatives |

## Internals

| Doc                                                              | Purpose                                               |
| ---------------------------------------------------------------- | ----------------------------------------------------- |
| [LUKS unlock](internals/luks-unlock.md)                          | Unlock, header backup, and recovery-message contract  |
| [Device disappearance](internals/tool-behavior/device-disappearance.md) | External-tool output for missing device states |
| [SATA hot-unplug](internals/real-world/sata-hot-unplug.md)       | Real hardware observations for hot-unplug behavior    |
| [btrfs notes](internals/btrfs/balance-profiles.md)               | btrfs RAID profile, balance, ENOSPC, and LUKS notes   |

## Development

| Doc                                  | Purpose                                      |
| ------------------------------------ | -------------------------------------------- |
| [Overview](dev/overview.md)          | Development workflow and dependency updates  |
| [Testing](dev/testing.md)            | VM test conventions and framework gotchas    |
| [TUI snapshots](dev/tui-snapshots.md) | Ratatui and Insta snapshot review workflow   |
