# braid manual

braid is a NixOS CLI tool for managing an encrypted btrfs RAID1 NAS. This manual covers every command and common workflows.

## Common tasks

- **First time setup** -- [Getting started](guides/getting-started.md)
- **Add, remove, or replace a disk** -- [add](commands/add.md), [remove](commands/remove.md), [replace](commands/replace.md)
- **USB auto-unlock** -- [Auto-unlock](guides/auto-unlock.md)
- **Set up disk health alerts** -- [Monitoring and alerts](guides/monitoring-and-alerts.md)
- **Suspend when idle, wake on demand** -- [Power management](guides/power-management.md)

## Guides

| Guide | Description |
| --- | --- |
| [Getting started](guides/getting-started.md) | First-time setup: find disks, create pool, unlock |
| [Day-to-day NAS usage](guides/day-to-day-nas-usage.md) | Subvolumes, file permissions, Samba shares |
| [Auto-unlock](guides/auto-unlock.md) | USB keyfile setup for unattended reboots |
| [Monitoring and alerts](guides/monitoring-and-alerts.md) | Disk health alerts, beeper, alert commands |
| [Power management](guides/power-management.md) | Auto-suspend, Wake-on-LAN, RTC wakeups |
| [NixOS configuration](guides/nixos-configuration.md) | Module options, scrub scheduling, pinned toolchain |
| [Sharing and permissions](guides/sharing-and-permissions.md) | Storage group, mount permissions, Samba |
| [Troubleshooting](guides/troubleshooting.md) | ENOSPC balance, paused balance, missing devices |
| [Recovery scenarios](guides/recovery-scenarios.md) | Interrupted operations, lost pool.json, degraded mount |

## Commands

| Command | Description |
| --- | --- |
| [add](commands/add.md) | Add disks to the pool (or create a new pool) |
| [remove](commands/remove.md) | Remove a live disk from the pool |
| [remove-missing](commands/remove-missing.md) | Forget a dead/missing device entry |
| [replace](commands/replace.md) | Replace a live or dead disk |
| [unlock](commands/unlock.md) | Open LUKS devices and mount the pool |
| [lock](commands/lock.md) | Unmount the pool and close LUKS devices |
| [idle](commands/idle.md) | Check if the pool is idle (for auto-suspend) |
| [status](commands/status.md) | Pool health, disk status, allocation, scrub info |
| [doctor](commands/doctor.md) | Diagnostic checks for config and pool health |
| [monitor](commands/monitor.md) | Health check for alerting (used by systemd timer) |
| [ack](commands/ack.md) | Acknowledge and silence an active alert |
| [enroll](commands/enroll.md) | Enroll a USB keyfile for auto-unlock |
| [discover](commands/discover.md) | Scan for braid LUKS devices and rebuild pool.json |
| [recover](commands/recover.md) | Recover from an interrupted operation |
| [tui](commands/tui.md) | Interactive dashboard |
| [browse](commands/browse.md) | Read-only browser for raw btrfs output |

## Development

See [development.md](development.md) for the dev workflow, test commands, and dependency upgrade process.
