# Summary

[braid](index.md)

# Guides

- [Install NixOS](guides/install-nixos.md)
- [Getting started](guides/getting-started.md)
- [Day-to-day NAS usage](guides/day-to-day-nas-usage.md)
- [Auto-unlock](guides/auto-unlock.md)
- [Monitoring and alerts](guides/monitoring-and-alerts.md)
- [Power management](guides/power-management.md)
- [Fan control](guides/fan-control.md)
- [UPS](guides/ups.md)
- [NixOS configuration](guides/nixos-configuration.md)
- [Sharing and permissions](guides/sharing-and-permissions.md)
- [Mounting subvolumes](guides/mounting-subvolumes.md)
- [Troubleshooting](guides/troubleshooting.md)
- [Recovery scenarios](guides/recovery-scenarios.md)

# Commands

- [add](commands/add.md)
- [remove](commands/remove.md)
- [remove-missing](commands/remove-missing.md)
- [replace](commands/replace.md)
- [unlock](commands/unlock.md)
- [lock](commands/lock.md)
- [seal-mountpoint 🧪](commands/seal-mountpoint.md)
- [idle 🧪](commands/idle.md)
- [status](commands/status.md)
- [doctor](commands/doctor.md)
- [monitor 🧪](commands/monitor.md)
- [ack 🧪](commands/ack.md)
- [enroll 🧪](commands/enroll.md)
- [discover 🧪](commands/discover.md)
- [recover 🧪](commands/recover.md)
- [tui](commands/tui.md)
- [ups status 🧪](commands/ups-status.md)

# Design

- [Principles](design/principles.md)

# Decisions

- [001: btrfs RAID1](design/decisions/001-btrfs-raid1.md)
- [002: Config-first workflow](design/decisions/002-config-first-workflow.md)
- [003: Resilient by default](design/decisions/003-resilient-boot.md)
- [004: Single passphrase](design/decisions/004-single-passphrase.md)
- [005: Sane defaults](design/decisions/005-sane-defaults.md)
- [006: NixOS-native](design/decisions/006-nix-native.md)
- [007: Disk pool management](design/decisions/007-disk-pool-management.md)
- [008: Unified CLI](design/decisions/008-unified-cli.md)
- [009: Safe-by-construction reconciliation](design/decisions/009-safe-by-construction-reconciliation.md)
- [010: Toolchain pinning](design/decisions/010-toolchain-pinning.md)
- [011: Two-phase apply](design/decisions/011-two-phase-apply.md)
- [012: Intent CLI](design/decisions/012-intent-cli.md)
- [013: Mount permissions](design/decisions/013-mount-permissions.md)
- [014: Alerts](design/decisions/014-alerts.md)
- [015: HDD defaults](design/decisions/015-hdd-defaults.md)
- [016: Auto-suspend](design/decisions/016-auto-suspend.md)
- [017: Runtime disk membership](design/decisions/017-runtime-disk-membership.md)
- [018: Systemd lifecycle](design/decisions/018-systemd-lifecycle.md)
- [019: Inhibit sleep](design/decisions/019-inhibit-sleep.md)
- [020: UPS integration](design/decisions/020-ups-integration.md)
- [021: Wait rows in unlock](design/decisions/021-wait-in-unlock.md)
- [022: Dry-run preview model](design/decisions/022-dry-run-preview-model.md)
- [023: Secret handling](design/decisions/023-secret-handling.md)
- [024: LUKS UUID identity](design/decisions/024-luks-uuid-identity.md)
- [025: Browse vs curated](design/decisions/025-browse-vs-curated.md)
- [026: Pool lock rust-owned](design/decisions/026-pool-lock-rust-owned.md)
- [027: mkfs block-group-tree](design/decisions/027-mkfs-block-group-tree.md)
- [028: Immutable unmounted mountpoint](design/decisions/028-immutable-unmounted-mountpoint.md)
- [029: Release process](design/decisions/029-release-process.md)
- [030: SMART/btrfs error reporting](design/decisions/030-smart-btrfs-error-reporting.md)
- [031: Drive-wake posture](design/decisions/031-drive-wake-posture.md)
- [032: Pool mount hardening](design/decisions/032-pool-mount-hardening.md)

# Internals

- [LUKS unlock](internals/luks-unlock.md)
- [Device disappearance](internals/tool-behavior/device-disappearance.md)
- [smartd alert conditions](internals/tool-behavior/smartd-alerts.md)
- [SATA hot-unplug](internals/real-world/sata-hot-unplug.md)
- [btrfs balance profiles](internals/btrfs/balance-profiles.md)
- [btrfs balance soft flag](internals/btrfs/balance-soft.md)
- [btrfs ENOSPC vs hang](internals/btrfs/enospc-vs-hang.md)
- [LUKS sector size and btrfs](internals/btrfs/luks-sector-size.md)
- [btrfs dev_replace resume](internals/btrfs/dev-replace-resume.md)

# Development

- [Overview](dev/overview.md)
- [Releasing](dev/releasing.md)
- [Testing](dev/testing.md)
- [Parser compatibility](dev/parser-compatibility.md)
- [Reference source](dev/reference-source.md)
- [TUI snapshots](dev/tui-snapshots.md)
- [Planning and review hygiene](dev/planning-hygiene.md)
- [Mutation safety heuristics](dev/safety-heuristics.md)
- [Doc and ADR file references](dev/doc-citations.md)
- [Rust doc comments](dev/doc-comments.md)
