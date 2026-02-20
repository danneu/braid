# braid

NixOS module for encrypted NAS storage with auto-healing and dynamic drive pooling.

- **LUKS** full disk encryption with SSH remote unlock
- **btrfs RAID1** with checksumming and automatic self-healing
- Dynamic pool — add or remove drives without reformatting

## Example

```nix
braid = {
  enable = true;
  disks = [
    "/dev/disk/by-id/ata-Toshiba_MN07_XXXX"
    "/dev/disk/by-id/ata-Ironwolf_ST12_YYYY"
  ];
  mountPoint = "/mnt/storage";  # default
};
```

This creates LUKS devices for each disk, assembles them into a btrfs RAID1 pool, and mounts it. The system boots gracefully even if a drive is dead or missing.

## Managing drives

Every drive operation follows the same pattern: **declare it in config first, then run the CLI tool.**

### Find your disks

```
ls /dev/disk/by-id/ata-*
```

Or run `braid-add-disk` with no arguments to see configured and available disks.

### Start with one disk

You don't need to wait for a second drive. Start with one and add redundancy later.

```nix
braid = {
  enable = true;
  disks = [ "/dev/disk/by-id/ata-Toshiba_MN07_XXXX" ];
};
```

```
sudo nixos-rebuild switch
sudo braid-add-disk /dev/disk/by-id/ata-Toshiba_MN07_XXXX
```

The pool is live immediately. No redundancy yet — data is available but unprotected until a second drive is added.

### Add a drive

Same pattern — declare, rebuild, format:

```nix
braid.disks = [
  "/dev/disk/by-id/ata-Toshiba_MN07_XXXX"
  "/dev/disk/by-id/ata-Ironwolf_ST12_YYYY"  # new
];
```

```
sudo nixos-rebuild switch
sudo braid-add-disk /dev/disk/by-id/ata-Ironwolf_ST12_YYYY
```

The pool converts to RAID1 automatically. Existing data rebalances in the background. The pool stays online the entire time.

### Remove a drive

<!-- TODO: braid-remove-disk not yet implemented -->

> **Not yet implemented.** See [`docs/decisions/disk-pool-management.md`](docs/decisions/disk-pool-management.md) for the design.

Same config-first pattern — remove from config, rebuild, then run CLI:

```nix
# remove from config
braid.disks = [
  "/dev/disk/by-id/ata-Toshiba_MN07_XXXX"
  # removed: ata-Ironwolf_ST12_YYYY
];
```

```
sudo nixos-rebuild switch
sudo braid-remove-disk /dev/disk/by-id/ata-Ironwolf_ST12_YYYY
```

Data migrates off the drive before it's detached. Requires enough free space on remaining drives.

### Replace a failed drive

If a drive dies, the pool stays mounted in degraded mode. Replace it by swapping the dead disk for the replacement in config:

```nix
braid.disks = [
  "/dev/disk/by-id/ata-Toshiba_MN07_XXXX"
  "/dev/disk/by-id/ata-Seagate_NEW_ZZZZ"  # replacement
  # removed: ata-Ironwolf_ST12_YYYY (dead)
];
```

```
sudo nixos-rebuild switch
sudo braid-add-disk /dev/disk/by-id/ata-Seagate_NEW_ZZZZ
```

The new drive joins the pool and the dead device is automatically evicted during rebalance. This uses `braid-add-disk` (already implemented and tested).

<!-- TODO: planned removal of a healthy disk uses braid-remove-disk (not yet implemented) -->

### Pool status

<!-- TODO: braid-status not yet implemented -->

> **Not yet implemented.** See [`docs/decisions/disk-pool-management.md`](docs/decisions/disk-pool-management.md) for the design.

```
sudo braid-status           # pool health summary
sudo braid-status --verbose  # per-disk detail
```

Shows drive health, pool usage, RAID profile, and last scrub result.

## What you get for free

Braid enables these automatically when `braid.enable = true`:

- **Monthly btrfs scrub** — detects and repairs bit rot before it can compound. Override or disable with normal NixOS config:

  ```nix
  # change to weekly
  services.btrfs.autoScrub.interval = "weekly";

  # or disable
  services.btrfs.autoScrub.enable = false;
  ```

- **Resilient boot** — a dead or missing drive never blocks boot. The pool mounts in degraded mode and the system stays reachable via SSH.

## Samba

Samba is not part of the braid module — NixOS already provides declarative Samba config. Reference `config.braid.mountPoint` to stay in sync:

```nix
services.samba = {
  enable = true;
  openFirewall = true;
  settings = {
    videos = {
      path = "${config.braid.mountPoint}/videos";
      "guest ok" = "yes";
      "read only" = "yes";
      browseable = "yes";
    };
    poetry = {
      path = "${config.braid.mountPoint}/poetry";
      "guest ok" = "no";
      "valid users" = "dan";
      browseable = "no";
    };
  };
};
```

Change `braid.mountPoint` and Samba follows automatically — no extra module code needed, just a Nix expression referencing an existing option.

After rebuilding, create the Samba password (one-time):

```
sudo smbpasswd -a dan
```

Then from macOS: Finder → Cmd+K → `smb://nas/videos`.
