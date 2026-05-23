[← braid](../index.md)

# Auto-unlock

This guide covers setting up unattended unlock with a USB keyfile so the pool comes online automatically at boot.

Read this if you want the NAS to unlock without SSH-ing in to type a passphrase -- for example, after a power outage or scheduled reboot.

## How it works

By default, braid requires a passphrase to unlock. Auto-unlock adds a binary keyfile stored on a USB drive as a second LUKS unlock method. At boot, braid mounts the USB, reads the keyfile, unlocks all drives, and unmounts the USB.

The passphrase (LUKS slot 0) still works for manual unlock. The keyfile lives in LUKS slot 1.

### Boot behavior

**With USB key present:** NAS boots, `braid-auto-unlock.service` runs, mounts USB, unlocks pool, unmounts USB. Pool is online by the time you can SSH in.

**Without USB key present:** The service waits for the USB device (up to `timeoutSec`, default 5 seconds), then skips gracefully. The pool stays locked. You SSH in and `sudo braid unlock` with your passphrase as usual.

This means removing the USB key is all it takes to go back to manual unlock.

## Step 1: Generate and enroll the keyfile

Plug in a USB drive and find its by-id path:

```sh
lsblk -d -o NAME,SIZE,MODEL,ID-LINK
```

Mount it somewhere temporary:

```sh
sudo mount /dev/disk/by-id/usb-SanDisk_Cruzer_XXXX-0:0-part1 /mnt/usb
```

Generate a random keyfile and enroll it into all pool disks:

```sh
sudo braid enroll /mnt/usb --generate
```

`--generate` requires `/mnt/usb` to already be mounted. This creates a 4096-byte random file at `/mnt/usb/braid.key` and enrolls it into LUKS slot 1 on every disk in the pool. braid asks for your existing passphrase to authorize the enrollment.

Unmount the USB:

```sh
sudo umount /mnt/usb
```

### Enroll during `braid add`

If you are adding a new disk and already have a keyfile on USB, you can enroll in one step:

```sh
sudo braid add newdisk=/dev/disk/by-id/ata-NEWDISK_SERIAL --enroll /mnt/usb
```

The `--enroll` flag points to the directory containing `braid.key`. The new disk gets both the passphrase and the keyfile.

## Step 2: Enable auto-unlock in NixOS config

Find your USB key's by-id path (use the raw device, not a partition, if your USB has no partition table):

```sh
lsblk -d -o NAME,SIZE,MODEL,ID-LINK
```

Add to your NixOS configuration:

```nix
# configuration.nix
braid = {
  enable = true;
  package = braid.packages.x86_64-linux.default;

  autoUnlock = {
    enable = true;
    keyDevice = "/dev/disk/by-id/usb-SanDisk_Cruzer_XXXX-0:0-part1";
  };
};
```

Rebuild:

```sh
sudo nixos-rebuild switch
```

### Configuration options

| Option | Default | Description |
| --- | --- | --- |
| `autoUnlock.enable` | `false` | Enable USB keyfile auto-unlock |
| `autoUnlock.keyDevice` | -- | Block device for the USB key (must use `/dev/disk/by-id/` path) |
| `autoUnlock.timeoutSec` | `5` | Seconds to wait for USB device before giving up |
| `autoUnlock.allowDegraded` | `false` | Mount even if some drives are missing (degraded mode) |

`keyDevice` must be a `/dev/disk/by-id/` path. braid rejects `/dev/sdX` paths because they can change between reboots.

### Degraded mode

By default, auto-unlock refuses to mount if any pool drive is missing. This prevents silent operation with zero redundancy.

If you want the pool to come online even with a missing drive (for example, if a drive has failed and you plan to replace it), set:

```nix
autoUnlock.allowDegraded = true;
```

New writes in degraded mode have no redundancy until the drive is replaced and data rebalances.

## Step 3: Test it

Reboot the NAS with the USB key plugged in:

```sh
sudo reboot
```

After boot, SSH in and check:

```sh
sudo braid status
```

The pool should be online. Check the journal to confirm auto-unlock ran:

```sh
journalctl -u braid-auto-unlock.service
```

Then test without the USB key: remove it, reboot, and confirm the pool stays locked until you manually unlock.

## Security considerations

The keyfile on the USB drive can unlock your pool without a passphrase. Treat it like a physical key:

- **Remove the USB after boot.** The auto-unlock service unmounts the USB immediately after reading the keyfile, but physically removing it ensures no one can copy the key from a running system.
- **Store the USB securely.** If someone has physical access to both the USB key and the NAS drives, they can decrypt your data.
- **Keep a backup of the keyfile.** If you lose the USB key, you still have your passphrase. But if you want another auto-unlock USB, you need to `braid enroll` again.

## LUKS header backups

After enrolling a keyfile, braid modifies the LUKS header on each drive (adding slot 1). braid stores LUKS header backups in `/var/lib/braid/luks-headers/` as a transient byproduct.

Copy each `.luksheader` file to a separate location (external drive, another machine), then delete the local file. `braid status` warns until the local copies are removed. If a drive's LUKS header is corrupted, the off-system backup is the only way to recover access to that drive's data.

## What's next

- [Monitoring and alerts](monitoring-and-alerts.md) -- get notified when a disk has problems
- [Power management](power-management.md) -- auto-suspend and Wake-on-LAN

## Related commands

- [enroll](../commands/enroll.md)
- [unlock](../commands/unlock.md)
- [add](../commands/add.md)
