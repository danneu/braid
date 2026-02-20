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

Every drive operation follows the same pattern: **edit config, rebuild, plan, apply.**

### Find your disks

```
ls /dev/disk/by-id/ata-*
```

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

Edit config, rebuild, preview, apply:

```nix
braid.disks = [
  "/dev/disk/by-id/ata-Toshiba_MN07_XXXX"
  "/dev/disk/by-id/ata-Ironwolf_ST12_YYYY"  # new
];
```

```
sudo nixos-rebuild switch
sudo braid plan      # preview what will happen
sudo braid apply     # execute it
```

The pool converts to RAID1 automatically. Existing data rebalances in the background. The pool stays online the entire time.

### Remove a drive

Same pattern — remove from config, rebuild, plan, apply:

```nix
braid.disks = [
  "/dev/disk/by-id/ata-Toshiba_MN07_XXXX"
  # removed: ata-Ironwolf_ST12_YYYY
];
```

```
sudo nixos-rebuild switch
sudo braid plan
sudo braid apply
```

Data migrates off the drive before it's detached. Requires enough free space on remaining drives. If removing would leave a single disk (losing redundancy), `braid apply` requires explicit confirmation.

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
sudo braid plan      # shows: add new disk, remove missing device
sudo braid apply
```

The new drive joins the pool and the dead device is evicted. The plan shows exactly what will happen before any changes are made.

### Pool status

```
sudo braid status             # pool health summary
sudo braid status --verbose   # per-disk detail
sudo braid status --json      # machine-readable output
```

Shows drive count, RAID profile, capacity, degraded/missing state, and last scrub result. With `--verbose`, adds per-disk detail: model, serial, LUKS UUID, and btrfs error counters.

### Resume an interrupted apply

If `braid apply` is interrupted (power loss, killed process), it saves a checkpoint. Resume where it left off:

```
sudo braid apply --resume
```

The checkpoint is validated against the current config — if the config changed since the interruption, resume is refused and you must start fresh.

### Standalone scripts

The original single-purpose scripts still work for backward compatibility:

```
sudo braid-add-disk /dev/disk/by-id/ata-...
sudo braid-remove-disk /dev/disk/by-id/ata-...
sudo braid-status [--verbose]
```

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
