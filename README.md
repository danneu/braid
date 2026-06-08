# braid

braid is a NixOS CLI tool for managing an encrypted, redundant NAS. It wraps two standard Linux tools into a simple interface:

- **[LUKS](https://en.wikipedia.org/wiki/Linux_Unified_Key_Setup)** -- full disk encryption (passphrase-based, keys never stored on disk)
- **[btrfs RAID1](https://btrfs.readthedocs.io/en/latest/)** -- checksumming filesystem with automatic self-healing from redundant copies

And it leans heavily on **[systemd](https://systemd.io/)**, built into NixOS: the
unlock/mount lifecycle, scrub timers, and UPS/fan/suspend services all run as
systemd units.

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

## Features

- **Full disk encryption** -- passphrase or USB keyfile to unlock
- **Redundancy** -- data stored on two disks; tolerates a single disk failure
- **Dynamic pool** -- add or remove drives with a command, no `nixos-rebuild`
- **Self-healing** -- btrfs checksums every block and silently repairs corruption from the redundant copy
- **Offline-write safety** -- the pool mountpoint is sealed immutable while the pool is unmounted, so a process writing it before the pool mounts fails loudly with `EPERM` instead of silently landing data on the root disk (which the pool would then hide on mount)
- **CLI-owned membership** -- `braid add`/`remove`/`replace` manage the pool; state lives in UUID-keyed `/var/lib/braid/pool.json`
- **UPS safety** -- with UPS support enabled, NUT drives orderly poweroff on low battery, mutating commands refuse to start unless UPS utility power is verified, and `braid ups status` / the TUI show live UPS state
- **TUI dashboard** -- `braid tui` shows pool health, disk status, balance progress, SMART data, and (when enabled) chassis fan telemetry plus UPS state

## Downsides

- **RAID1 capacity cost** -- half your raw capacity goes to redundancy. Four 12 TB drives = 24 TB usable.
- **HDD-first** -- defaults are tuned for spinning drives (e.g. no TRIM). SSDs may work but are not supported.
- **Unstable** -- this is pre-v1.0 and I change things when I decide on a better way. Commands, flags, config, and even on-disk state like `pool.json` format can change.
- **Unproven** -- I run braid on a daily-use 4x12TB NAS, and there are 180+ NixOS VM tests and 2200+ Rust tests, but there are almost certainly weird/bad edge cases. That said, every mutating command takes a `--dry-run` flag, so you can preview exactly what it'll do before it touches your disks.

## Install

> NixOS/Linux only (x86_64). The CLI wraps Linux storage tooling (LUKS, btrfs,
> systemd) and does not run on macOS.

Try it without installing anything:

```sh
nix run github:danneu/braid?ref=release -- --help
```

Add braid to your flake inputs and import the module:

```nix
# flake.nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
    braid.url = "github:danneu/braid?ref=release";
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

`?ref=release` tracks braid's release channel (`nix flake update braid` upgrades).
Pin a fixed version instead with a tag or commit:

```nix
# flake.nix
braid.url = "github:danneu/braid?ref=v0.0.1";    # pin a tag
braid.url = "github:danneu/braid?rev=<commit>";  # pin an exact commit
```

Add braid's public binary cache so the NAS pulls the prebuilt CLI instead of
recompiling -- this relies on the no-`follows` input above, which keeps braid on
its pinned nixpkgs:

```nix
# configuration.nix
nix.settings = {
  extra-substituters = [ "https://braid.cachix.org" ];
  extra-trusted-public-keys = [ "braid.cachix.org-1:I/p7fx1z5n0+O80KzMuT7aXRdkVyHr/buZKaBu7HvJs=" ];
};
```

## Previewing and confirming changes

### Preview with --dry-run

Add `--dry-run` to any pool-lifecycle command to print the exact plan -- every
LUKS, btrfs, and mount step it would run -- without touching your disks. Each
step is tagged `[destructive]`, `[safe]`, or `[long]` (a long-running step like a
btrfs balance), so you can see at a glance what each step does, and the indented
`$` line is the literal command:

```
sudo braid add ironwolf=/dev/disk/by-id/ata-Ironwolf_ST12_YYYY \
               toshiba=/dev/disk/by-id/ata-Toshiba_MN07_XXXX --dry-run

[destructive] LUKS format /dev/disk/by-id/ata-Ironwolf_ST12_YYYY
               $ cryptsetup luksFormat --type luks2 --batch-mode '--key-file=-' --uuid 7f9d2e4a-1c3b-4f5a-8e6d-2a1b3c4d5e6f --label braid-ironwolf /dev/disk/by-id/ata-Ironwolf_ST12_YYYY
[safe]        LUKS header backup -> /var/lib/braid/luks-headers/braid-ironwolf.luksheader
               $ cryptsetup luksHeaderBackup --header-backup-file /var/lib/braid/luks-headers/braid-ironwolf.luksheader /dev/disk/by-id/ata-Ironwolf_ST12_YYYY
[safe]        LUKS open -> braid-ironwolf
               $ cryptsetup open --type luks '--key-file=-' --perf-no_read_workqueue --perf-no_write_workqueue /dev/disk/by-id/ata-Ironwolf_ST12_YYYY braid-ironwolf
[destructive] LUKS format /dev/disk/by-id/ata-Toshiba_MN07_XXXX
               $ cryptsetup luksFormat --type luks2 --batch-mode '--key-file=-' --uuid 3a8c1d9f-5e2b-4a7c-9f1e-6b4d2c8a0e3f --label braid-toshiba /dev/disk/by-id/ata-Toshiba_MN07_XXXX
[safe]        LUKS header backup -> /var/lib/braid/luks-headers/braid-toshiba.luksheader
               $ cryptsetup luksHeaderBackup --header-backup-file /var/lib/braid/luks-headers/braid-toshiba.luksheader /dev/disk/by-id/ata-Toshiba_MN07_XXXX
[safe]        LUKS open -> braid-toshiba
               $ cryptsetup open --type luks '--key-file=-' --perf-no_read_workqueue --perf-no_write_workqueue /dev/disk/by-id/ata-Toshiba_MN07_XXXX braid-toshiba
[safe]        mkfs.btrfs RAID1 /dev/mapper/braid-ironwolf /dev/mapper/braid-toshiba
               $ mkfs.btrfs -d raid1 -m raid1 -O block-group-tree /dev/mapper/braid-ironwolf /dev/mapper/braid-toshiba
[safe]        mount -> /mnt/storage
               $ mount -o 'noatime,skip_balance,subvolid=5' /dev/mapper/braid-ironwolf /mnt/storage
```

### Confirm before it runs

Without `--dry-run`, the data-shape commands (`add`, `remove`, `remove-missing`,
`replace`) show what they are about to do and wait for you to type `yes` --
anything else aborts:

```
sudo braid remove ironwolf

Remove from pool:
  ironwolf  Seagate IronWolf | 12.00 TiB | serial ZL2A1B2C
            devid 2 | data will migrate to remaining disks

Pool: 3 disks -> 2 disks

Type 'yes' to continue:
```

Pass `--yes` to skip the prompt (for scripts and automation):

```
sudo braid remove ironwolf --yes
```

## Recovery

If a mutation is interrupted, braid leaves `/var/lib/braid/pending-op.json` in place and normal commands refuse until recovery completes. Run `sudo braid recover` (add `--allow-degraded` when a member is missing). Recovery repairs `pool.json` from committed live btrfs membership and, when btrfs balance state is idle, finishes only the owed post-mutation maintenance, such as resize or soft RAID1 balance. If owed RAID1 replay finds a paused, running, or unknown balance state, recover fails closed and preserves `pending-op.json` for manual inspection.

`pool.json` is keyed by each member's LUKS UUID. Disk names are still the names you type in commands and see in output; by-id paths are the hardware addresses braid uses to find disks.

## Docs

### Commands

| Command                                           | Description                                                   |
| ------------------------------------------------- | ------------------------------------------------------------- |
| [add](docs/commands/add.md)                       | Add disks to the pool (or create a new pool)                  |
| [remove](docs/commands/remove.md)                 | Remove a live disk from the pool                              |
| [remove-missing](docs/commands/remove-missing.md) | Forget a dead/missing device entry                            |
| [replace](docs/commands/replace.md)               | Replace a live or dead disk                                   |
| [unlock](docs/commands/unlock.md)                 | Open LUKS devices and mount the pool                          |
| [lock](docs/commands/lock.md)                     | Unmount the pool and close LUKS devices                       |
| [seal-mountpoint](docs/commands/seal-mountpoint.md) | Seal the offline mountpoint immutable (boot-managed; manual lever) |
| [idle](docs/commands/idle.md)                     | Check if the pool is idle (for auto-suspend)                  |
| [status](docs/commands/status.md)                 | Pool health, disk status, allocation, scrub info              |
| [doctor](docs/commands/doctor.md)                 | Diagnostic checks for config, pool health, and runtime safety |
| [monitor](docs/commands/monitor.md)               | Health check for alerting (used by systemd timer)             |
| [ack](docs/commands/ack.md)                       | Acknowledge and silence an active alert                       |
| [enroll](docs/commands/enroll.md)                 | Enroll a USB keyfile for auto-unlock                          |
| [discover](docs/commands/discover.md)             | Scan for braid LUKS devices and rebuild pool.json             |
| [recover](docs/commands/recover.md)               | Recover from an interrupted operation                         |
| [tui](docs/commands/tui.md)                       | Interactive dashboard with raw-output Browse tab              |
| [ups status](docs/commands/ups-status.md)         | Live UPS state (NUT); `--json` for scripts                    |

### Guides

| Guide                                                             | Description                                                |
| ----------------------------------------------------------------- | ---------------------------------------------------------- |
| [Install NixOS](docs/guides/install-nixos.md)                     | Install NixOS itself before setting up braid               |
| [Getting started](docs/guides/getting-started.md)                 | First-time setup: find disks, create pool, unlock          |
| [Day-to-day NAS usage](docs/guides/day-to-day-nas-usage.md)       | Subvolumes, file permissions, Samba shares                 |
| [Auto-unlock](docs/guides/auto-unlock.md)                         | USB keyfile setup for unattended reboots                   |
| [Monitoring and alerts](docs/guides/monitoring-and-alerts.md)     | Disk health alerts, beeper, alert commands                 |
| [Power management](docs/guides/power-management.md)               | Auto-suspend, Wake-on-LAN, RTC wakeups                     |
| [Fan control](docs/guides/fan-control.md)                         | HDD-driven chassis fan control via hddfancontrol           |
| [UPS](docs/guides/ups.md)                                         | NUT-backed orderly poweroff, preflight safety, live status |
| [NixOS configuration](docs/guides/nixos-configuration.md)         | Module options, scrub scheduling, pinned toolchain         |
| [Sharing and permissions](docs/guides/sharing-and-permissions.md) | Storage group, mount permissions, Samba                    |
| [Mounting subvolumes](docs/guides/mounting-subvolumes.md)         | Expose a btrfs subvolume at a custom path                  |
| [Troubleshooting](docs/guides/troubleshooting.md)                 | ENOSPC balance, paused balance, missing devices            |
| [Recovery scenarios](docs/guides/recovery-scenarios.md)           | Interrupted operations, lost pool.json, degraded mount     |

### Development

See [docs/dev/overview.md](docs/dev/overview.md) for the dev workflow, test commands, and dependency upgrade process.

## How braid is built

braid is written almost entirely by AI agents. After 20 years of software work, this project is my attempt at finding a state-of-the-art approach to AI-heavy engineering.

It is not vibe-coded. Every change runs through a deliberate plan-first pipeline: I generally have Claude Code (`--effort max`) draft a plan, then I run a revision loop with other agents -- I answer their clarifying questions, choose among branching decisions, and double-check the direction -- until the plan is ratcheted into a final form. Then an agent implements the plan.

The plan file is the main unit of work in braid. That is where all of my attention is spent. Implementation is derived from the plan.

Contributors, if any, would submit plan files rather than code, then we would revision-cycle the plan until it's ready for agent implementation.

## License

[MIT](LICENSE)
