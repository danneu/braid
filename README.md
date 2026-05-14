# braid

braid is a NixOS CLI tool for managing an encrypted, redundant NAS. It wraps two standard Linux tools into a simple interface:

- **[LUKS](https://en.wikipedia.org/wiki/Linux_Unified_Key_Setup)** -- full disk encryption (passphrase-based, keys never stored on disk)
- **[btrfs RAID1](https://btrfs.readthedocs.io/en/latest/)** -- checksumming filesystem with automatic self-healing from redundant copies

## Example

```sh
# Create a pool from three disks
sudo braid add \
  toshiba1=/dev/disk/by-id/aaa \
  toshiba2=/dev/disk/by-id/bbb \
  toshiba3=/dev/disk/by-id/ccc

# Unlock and mount at /mnt/storage
sudo braid unlock

# Use it
cp photos/* /mnt/storage/photos/

# Remove a disk (data migrates off first)
sudo braid remove toshiba3

# Replace a disk (inherits the old slot)
sudo braid replace --old toshiba2 \
  --new toshiba3=/dev/disk/by-id/ata-TOSHIBA_NEW_SERIAL

# Lock (unmount, close LUKS)
sudo braid lock
```

## Features

- **Full disk encryption** -- passphrase or USB keyfile to unlock
- **Redundancy** -- data stored on two disks; tolerates a single disk failure
- **Dynamic pool** -- add or remove drives with a command, no `nixos-rebuild`
- **Self-healing** -- btrfs checksums every block and silently repairs corruption from the redundant copy
- **CLI-owned membership** -- `braid add`/`remove`/`replace` manage the pool; state lives in UUID-keyed `/var/lib/braid/pool.json`
- **UPS safety** -- with `braid.ups.enable = true`, NUT drives orderly poweroff on low battery, mutating commands refuse to start while on battery, and `braid ups status` / the TUI show live UPS state
- **Dashboard** -- `braid tui` shows pool health, disk status, balance progress, SMART data, and (when enabled) chassis fan telemetry plus UPS state

## Downsides

- **RAID1 capacity cost** -- half your raw capacity goes to redundancy. Four 12 TB drives = 24 TB usable.
- **HDD-first** -- defaults are tuned for spinning drives (no TRIM, HDD scrub scheduling). SSDs may work but are not validated.

## Install

Add braid to your flake inputs and import the module:

```nix
# flake.nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
    braid.url = "github:danneu/braid";
    braid.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs = { nixpkgs, braid, ... }: {
    nixosConfigurations.myhost = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        braid.nixosModules.default
        ./configuration.nix
      ];
    };
  };
}
```

```nix
# configuration.nix
braid = {
  enable = true;
  mountPoint = "/mnt/storage";  # default
};
```

## Quick start

```sh
# Find your disks
lsblk -d -o NAME,SIZE,MODEL,ID-LINK

# Add disks to the pool
sudo braid add toshiba=/dev/disk/by-id/ata-Toshiba_MN07_XXXX \
               ironwolf=/dev/disk/by-id/ata-Ironwolf_ST12_YYYY

# Unlock after boot
sudo braid unlock

# Check pool health
sudo braid status

# Remove a disk
sudo braid remove ironwolf

# Replace a failed disk
sudo braid replace --old ironwolf --new seagate=/dev/disk/by-id/ata-Seagate_NEW_ZZZZ

# Lock the pool
sudo braid lock
```

See the [command reference](manual/commands/) for full usage of each command.

## Preview with --dry-run

Every mutating command (`add`, `remove`, `remove-missing`, `replace`, `unlock`, `lock`, `recover`, `enroll`) takes `--dry-run`. A successful dry-run prints one complete preview to stdout -- warnings that qualify the preview are part of it. Real runs may still print confirmations, progress, and failures to stderr.

`--dry-run` may also emit canonical `[wait]`/`[ok]`/`[skip]` status rows to stderr around any long-running probe that runs during preview generation -- for example, `braid enroll --dry-run` runs a passphrase-free `cryptsetup open --test-passphrase --key-file` against each disk to detect already-enrolled state, and announces that Argon2-bounded probe per Principle 13 ("announce long-running work"). These rows do not count as preview output -- the structured preview still lives entirely on stdout, and stderr is otherwise quiet on success.

```sh
sudo braid add toshiba=/dev/disk/by-id/ata-Toshiba_MN07_XXXX --dry-run
```

## Recovery

If a mutation is interrupted, braid leaves `/var/lib/braid/pending-op.json` in place and normal commands refuse until recovery completes. Run `sudo braid recover` (add `--allow-degraded` when a member is missing). Recovery repairs `pool.json` from committed live btrfs membership and finishes only the owed post-mutation maintenance, such as resize or soft RAID1 balance.

`pool.json` is keyed by each member's LUKS UUID. Disk names are still the names you type in commands and see in output; by-id paths are the hardware addresses braid uses to find disks.

## Manual

### Commands

| Command | Description |
| --- | --- |
| [add](manual/commands/add.md) | Add disks to the pool (or create a new pool) |
| [remove](manual/commands/remove.md) | Remove a live disk from the pool |
| [remove-missing](manual/commands/remove-missing.md) | Forget a dead/missing device entry |
| [replace](manual/commands/replace.md) | Replace a live or dead disk |
| [unlock](manual/commands/unlock.md) | Open LUKS devices and mount the pool |
| [lock](manual/commands/lock.md) | Unmount the pool and close LUKS devices |
| [status](manual/commands/status.md) | Pool health, disk status, allocation, scrub info |
| [doctor](manual/commands/doctor.md) | Diagnostic checks for config and pool health |
| [monitor](manual/commands/monitor.md) | Health check for alerting (used by systemd timer) |
| [ack](manual/commands/ack.md) | Acknowledge and silence an active alert |
| [enroll](manual/commands/enroll.md) | Enroll a USB keyfile for auto-unlock |
| [discover](manual/commands/discover.md) | Scan for braid LUKS devices and rebuild pool.json |
| [recover](manual/commands/recover.md) | Recover from an interrupted operation |
| [idle](manual/commands/idle.md) | Check if the pool is idle (for auto-suspend) |
| [tui](manual/commands/tui.md) | Interactive dashboard with raw-output Browse tab |
| [ups status](manual/commands/ups-status.md) | Live UPS state (NUT); `--json` for scripts |

### Guides

| Guide | Description |
| --- | --- |
| [Getting started](manual/guides/getting-started.md) | First-time setup: find disks, create pool, unlock |
| [Day-to-day NAS usage](manual/guides/day-to-day-nas-usage.md) | Subvolumes, file permissions, Samba shares |
| [Auto-unlock](manual/guides/auto-unlock.md) | USB keyfile setup for unattended reboots |
| [Monitoring and alerts](manual/guides/monitoring-and-alerts.md) | Disk health alerts, beeper, alert commands |
| [Power management](manual/guides/power-management.md) | Auto-suspend, Wake-on-LAN, RTC wakeups |
| [Fan control](manual/guides/fan-control.md) | HDD-driven chassis fan control via hddfancontrol |
| [UPS](manual/guides/ups.md) | NUT-backed orderly poweroff, preflight safety, live status |
| [NixOS configuration](manual/guides/nixos-configuration.md) | Module options, scrub scheduling, pinned toolchain |
| [Sharing and permissions](manual/guides/sharing-and-permissions.md) | Storage group, mount permissions, Samba |
| [Troubleshooting](manual/guides/troubleshooting.md) | ENOSPC balance, paused balance, missing devices |
| [Recovery scenarios](manual/guides/recovery-scenarios.md) | Interrupted operations, lost pool.json, degraded mount |

### Development

See [manual/development.md](manual/development.md) for the dev workflow, test commands, and dependency upgrade process.
