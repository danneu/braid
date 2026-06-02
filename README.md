# braid

braid is a NixOS CLI tool for managing an encrypted, redundant NAS. It wraps two standard Linux tools into a simple interface:

- **[LUKS](https://en.wikipedia.org/wiki/Linux_Unified_Key_Setup)** -- full disk encryption (passphrase-based, keys never stored on disk)
- **[btrfs RAID1](https://btrfs.readthedocs.io/en/latest/)** -- checksumming filesystem with automatic self-healing from redundant copies

And it leans heavily on **[systemd](https://systemd.io/)**, built into NixOS: the
unlock/mount lifecycle, scrub timers, and UPS/fan/suspend services all run as
systemd units.

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

> NixOS/Linux only (x86_64). The CLI wraps Linux storage tooling (LUKS, btrfs,
> systemd) and does not run on macOS.

Try it without installing anything:

```sh
nix run github:danneu/braid -- --help
```

Add braid to your flake inputs and import the module:

```nix
# flake.nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
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

See the [command reference](docs/commands/) for full usage of each command.

## Preview with --dry-run

Every pool-lifecycle command (`add`, `remove`, `remove-missing`, `replace`, `unlock`, `lock`, `recover`, `enroll`) takes `--dry-run`. A successful dry-run prints one complete preview to stdout -- warnings that qualify the preview are part of it. Real runs may still print confirmations, progress, and failures to stderr.

`--dry-run` may also emit canonical `[wait]`/`[ok]`/`[skip]` status rows to stderr around any long-running probe that runs during preview generation -- for example, `braid enroll --dry-run` runs a passphrase-free `cryptsetup open --test-passphrase --key-file` against each disk to detect already-enrolled state, and announces that Argon2-bounded probe per Principle 13 ("announce long-running work"). These rows do not count as preview output -- the structured preview still lives entirely on stdout, and stderr is otherwise quiet on success.

```sh
sudo braid add toshiba=/dev/disk/by-id/ata-Toshiba_MN07_XXXX --dry-run
```

## Recovery

If a mutation is interrupted, braid leaves `/var/lib/braid/pending-op.json` in place and normal commands refuse until recovery completes. Run `sudo braid recover` (add `--allow-degraded` when a member is missing). Recovery repairs `pool.json` from committed live btrfs membership and finishes only the owed post-mutation maintenance, such as resize or soft RAID1 balance.

`pool.json` is keyed by each member's LUKS UUID. Disk names are still the names you type in commands and see in output; by-id paths are the hardware addresses braid uses to find disks.

## Docs

### Commands

| Command | Description |
| --- | --- |
| [add](docs/commands/add.md) | Add disks to the pool (or create a new pool) |
| [remove](docs/commands/remove.md) | Remove a live disk from the pool |
| [remove-missing](docs/commands/remove-missing.md) | Forget a dead/missing device entry |
| [replace](docs/commands/replace.md) | Replace a live or dead disk |
| [unlock](docs/commands/unlock.md) | Open LUKS devices and mount the pool |
| [lock](docs/commands/lock.md) | Unmount the pool and close LUKS devices |
| [idle](docs/commands/idle.md) | Check if the pool is idle (for auto-suspend) |
| [status](docs/commands/status.md) | Pool health, disk status, allocation, scrub info |
| [doctor](docs/commands/doctor.md) | Diagnostic checks for config, pool health, and runtime safety |
| [monitor](docs/commands/monitor.md) | Health check for alerting (used by systemd timer) |
| [ack](docs/commands/ack.md) | Acknowledge and silence an active alert |
| [enroll](docs/commands/enroll.md) | Enroll a USB keyfile for auto-unlock |
| [discover](docs/commands/discover.md) | Scan for braid LUKS devices and rebuild pool.json |
| [recover](docs/commands/recover.md) | Recover from an interrupted operation |
| [tui](docs/commands/tui.md) | Interactive dashboard with raw-output Browse tab |
| [ups status](docs/commands/ups-status.md) | Live UPS state (NUT); `--json` for scripts |

### Guides

| Guide | Description |
| --- | --- |
| [Install NixOS](docs/guides/install-nixos.md) | Install NixOS itself before setting up braid |
| [Getting started](docs/guides/getting-started.md) | First-time setup: find disks, create pool, unlock |
| [Day-to-day NAS usage](docs/guides/day-to-day-nas-usage.md) | Subvolumes, file permissions, Samba shares |
| [Auto-unlock](docs/guides/auto-unlock.md) | USB keyfile setup for unattended reboots |
| [Monitoring and alerts](docs/guides/monitoring-and-alerts.md) | Disk health alerts, beeper, alert commands |
| [Power management](docs/guides/power-management.md) | Auto-suspend, Wake-on-LAN, RTC wakeups |
| [Fan control](docs/guides/fan-control.md) | HDD-driven chassis fan control via hddfancontrol |
| [UPS](docs/guides/ups.md) | NUT-backed orderly poweroff, preflight safety, live status |
| [NixOS configuration](docs/guides/nixos-configuration.md) | Module options, scrub scheduling, pinned toolchain |
| [Sharing and permissions](docs/guides/sharing-and-permissions.md) | Storage group, mount permissions, Samba |
| [Mounting subvolumes](docs/guides/mounting-subvolumes.md) | Expose a btrfs subvolume at a custom path |
| [Troubleshooting](docs/guides/troubleshooting.md) | ENOSPC balance, paused balance, missing devices |
| [Recovery scenarios](docs/guides/recovery-scenarios.md) | Interrupted operations, lost pool.json, degraded mount |

### Development

See [docs/dev/overview.md](docs/dev/overview.md) for the dev workflow, test commands, and dependency upgrade process.

## License

[MIT](LICENSE)
