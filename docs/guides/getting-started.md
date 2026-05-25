[← braid](../index.md)

# Getting started

This guide walks you through first-time braid setup: installing the NixOS module, finding your disks, creating a pool, and unlocking it.

Read this if you have a fresh NixOS machine with empty drives and want to set up an encrypted RAID1 NAS.

## What braid manages

braid owns two things:

- **LUKS encryption** -- each drive is individually encrypted with a shared passphrase. Keys are never stored on disk.
- **btrfs RAID1** -- your encrypted drives form a single filesystem with automatic redundancy and self-healing checksums.

The NixOS module provides the systemd units, mount point, and toolchain. The CLI owns which disks are in the pool -- adding or removing a drive is a `braid` command, not a `nixos-rebuild`.

Pool membership lives in `/var/lib/braid/pool.json`. This file is created by `braid add` and read by `braid unlock`. It is keyed by each member's LUKS UUID; the disk name is stored inside each entry for commands and display.

## Install the NixOS module

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

Minimal configuration:

```nix
# configuration.nix
braid = {
  enable = true;
  mountPoint = "/mnt/storage";  # default
};
```

Only `braid.enable = true` is required. `nixosModules.default` defaults
`braid.package` to braid's pinned `braid-cli-unwrapped`; `mountPoint` defaults
to `/mnt/storage`.

Rebuild and switch:

```sh
sudo nixos-rebuild switch
```

## Find your disks

Use `lsblk` to identify the drives you want to add:

```sh
lsblk -d -o NAME,SIZE,MODEL,ID-LINK
```

Example output:

```
NAME  SIZE MODEL               ID-LINK
sda   12T  TOSHIBA MN07ACA12T  ata-TOSHIBA_MN07ACA12T_XXXX
sdb   12T  TOSHIBA MN07ACA12T  ata-TOSHIBA_MN07ACA12T_YYYY
sdc   12T  TOSHIBA MN07ACA12T  ata-TOSHIBA_MN07ACA12T_ZZZZ
sdd  500G  Samsung SSD 860     ata-Samsung_SSD_860_AAAA      # boot drive -- leave this alone
```

You need the `ID-LINK` values. braid always uses `/dev/disk/by-id/` paths -- never `/dev/sdX`, which can change between reboots.

## Create the pool

Add your drives with `braid add`. Each drive gets a short name you choose and a by-id path:

```sh
sudo braid add \
  toshiba1=/dev/disk/by-id/ata-TOSHIBA_MN07ACA12T_XXXX \
  toshiba2=/dev/disk/by-id/ata-TOSHIBA_MN07ACA12T_YYYY \
  toshiba3=/dev/disk/by-id/ata-TOSHIBA_MN07ACA12T_ZZZZ
```

braid will:

1. Ask you to set a LUKS passphrase (used for all drives).
2. Format each drive with LUKS encryption.
3. Create a btrfs RAID1 filesystem across all drives.
4. Mount the pool at `/mnt/storage`.
5. Write pool membership to `/var/lib/braid/pool.json`.

All drives join the same btrfs RAID1 filesystem. btrfs RAID1 keeps exactly 2 copies of every block regardless of how many drives you add, so the pool tolerates a single drive failure -- a 3-drive pool tolerates the same single failure as a 2-drive pool, with more usable capacity. See [Day-to-day usage](day-to-day-nas-usage.md) for what additional drives buy you and how to add them later.

The disk names (`toshiba1`, `toshiba2`, etc.) are permanent presentation labels used in all future commands. Pick something short and meaningful. braid uses the LUKS UUID, not the name or LUKS label, as the persistent disk identity.

After `braid add` completes, the pool is online and mounted. You can start using it immediately:

```sh
ls /mnt/storage/
cp photos/* /mnt/storage/photos/
```

## Unlock after reboot

When the NAS reboots, the pool is offline -- LUKS drives are closed and nothing is mounted. This is by design: your data stays encrypted until you explicitly unlock it.

SSH into the NAS and unlock:

```sh
ssh user@nas
sudo braid unlock
```

braid prompts for your LUKS passphrase, opens all drives, assembles the btrfs pool, and mounts it.

## Check pool health

```sh
sudo braid status
```

This shows:

- Pool state (online/offline)
- Each disk's health and LUKS status
- Disk allocation and free space
- Scrub status
- Any active alerts

## Lock the pool

When you want to take the pool offline (unmount and close LUKS):

```sh
sudo braid lock
```

This is optional -- the pool locks automatically on shutdown. Manual locking is useful before maintenance or if you want to ensure the drives are encrypted at rest while the machine stays on.

## What's next

- [Day-to-day NAS usage](day-to-day-nas-usage.md) -- the reboot/unlock/use cycle, subvolumes, good habits
- [Auto-unlock](auto-unlock.md) -- USB keyfile for unattended reboots
- [Monitoring and alerts](monitoring-and-alerts.md) -- get notified when a disk has problems
- [Power management](power-management.md) -- auto-suspend and Wake-on-LAN

## Related commands

- [add](../commands/add.md)
- [unlock](../commands/unlock.md)
- [lock](../commands/lock.md)
- [status](../commands/status.md)
