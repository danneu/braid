# Passive detection of disk removal in a braid pool

How does the system react when a SATA drive disappears from a braid pool (LUKS + btrfs RAID1)?
Tested by hot-unplugging one of three SATA HDDs from a live, mounted pool.

## Setup

- 3x SATA HDDs, each LUKS-encrypted, assembled into a single btrfs RAID1 at `/mnt/storage`
- Unplugged disk: `wwn-0x5000c500ba0a8b52` (`sda`, LUKS label `braid-ccc`)

## Detection signals

| Signal                                     | Latency                   | Passive?    | Programmatic detection          |
| ------------------------------------------ | ------------------------- | ----------- | ------------------------------- |
| `ata*: SATA link down` (kernel journal)    | **Instant**               | Yes         | `journalctl -kf` pattern match  |
| udev `remove` event                        | ~11s (after SATA retries) | Yes         | udev rule on `ACTION=="remove"` |
| `/dev/disk/by-id/wwn-*` symlink disappears | ~11s (udev cleans it)     | Yes         | inotify on `/dev/disk/by-id/`   |
| `cryptsetup status` shows `device: (null)` | ~11s                      | Yes         | poll `cryptsetup status`        |
| btrfs write errors (periodic commit)       | ~26s                      | Yes         | `journalctl -kf` pattern match  |
| `btrfs device stats` shows nonzero errors  | ~26s+                     | Needs query | `btrfs device stats`            |

Key takeaway: the kernel journal and udev events are the fastest passive signals.
btrfs is completely oblivious until its next periodic commit (~30s default), but then notices on its own without user-initiated I/O.

The udev remove event is especially useful — it includes `ID_WWN` and `ID_FS_LABEL` (e.g. `braid-ccc`), so a udev rule can immediately identify which braid disk disappeared.

## What does NOT react

- **LUKS mapper** (`/dev/mapper/braid-ccc`): stays as a zombie. `cryptsetup status` still says "active" but the backing `device:` becomes `(null)`. I/O through it fails.
- **`btrfs filesystem show`**: continues to list all 3 devices with paths and sizes even after errors. Never reports the device as missing from this command alone.

## Real-world logs

### Kernel journal

SATA link-down is instant on unplug. The kernel retries a few times over ~11 seconds, then detaches the SCSI device:

```
Mar 21 12:08:37 silverstone kernel: ata2: SATA link down (SStatus 0 SControl 300)
Mar 21 12:08:43 silverstone kernel: ata2: SATA link down (SStatus 0 SControl 300)
Mar 21 12:08:43 silverstone kernel: ata2: limiting SATA link speed to <unknown>
Mar 21 12:08:48 silverstone kernel: ata2: SATA link down (SStatus 0 SControl 3F0)
Mar 21 12:08:48 silverstone kernel: ata2.00: disable device
Mar 21 12:08:48 silverstone kernel: ata2.00: detaching (SCSI 1:0:0:0)
Mar 21 12:08:48 silverstone kernel: sd 1:0:0:0: [sda] Synchronizing SCSI cache
Mar 21 12:08:48 silverstone kernel: sd 1:0:0:0: [sda] Synchronize Cache(10) failed: Result: hostbyte=DID_BAD_TARGET driverbyte=DRIVER_OK
Mar 21 12:08:48 silverstone kernel: sd 1:0:0:0: [sda] Stopping disk
Mar 21 12:08:48 silverstone kernel: sd 1:0:0:0: [sda] Start/Stop Unit failed: Result: hostbyte=DID_BAD_TARGET driverbyte=DRIVER_OK
```

~26 seconds after unplug, btrfs's periodic commit hits the dead device — no user I/O needed:

```
Mar 21 12:09:03 silverstone kernel: kworker/u64:3: attempt to access beyond end of device
                                    sda: rw=4097, sector=51744, nr_sectors = 32 limit=0
Mar 21 12:09:03 silverstone kernel: BTRFS error (device dm-0): bdev /dev/mapper/braid-ccc errs: wr 1, rd 0, flush 0, corrupt 0, gen 0
Mar 21 12:09:03 silverstone kernel: BTRFS error (device dm-0): bdev /dev/mapper/braid-ccc errs: wr 2, rd 0, flush 0, corrupt 0, gen 0
...
Mar 21 12:09:04 silverstone kernel: BTRFS warning (device dm-0): lost super block write due to IO error on /dev/mapper/braid-ccc (-5)
Mar 21 12:09:04 silverstone kernel: BTRFS error (device dm-0): error writing primary super block to device 3
```

### udev remove event

Arrives after the SATA retries complete (~11s). Includes disk identity:

```
KERNEL[1395.061297] remove   /devices/pci0000:00/0000:00:01.2/0000:02:00.1/ata2/host1/target1:0:0/1:0:0:0/block/sda (block)
ACTION=remove
DEVNAME=/dev/sda
DEVTYPE=disk

UDEV  [1395.091944] remove   /devices/pci0000:00/0000:00:01.2/0000:02:00.1/ata2/host1/target1:0:0/1:0:0:0/block/sda (block)
ACTION=remove
DEVNAME=/dev/sda
ID_WWN=0x5000c500ba0a8b52
ID_FS_LABEL=braid-ccc
ID_FS_TYPE=crypto_LUKS
DEVLINKS=... /dev/disk/by-id/wwn-0x5000c500ba0a8b52 ... /dev/disk/by-label/braid-ccc ...
```

### cryptsetup status (zombie mapper)

After the block device is gone, the LUKS mapper lingers but its backing device is null:

```
/dev/mapper/braid-ccc is active and is in use.
  type:    n/a
  cipher:  aes-xts-plain64
  device:  (null)
  mode:    read/write
```

### btrfs device stats (after errors)

```
[/dev/mapper/braid-ccc].write_io_errs    10
[/dev/mapper/braid-ccc].read_io_errs     0
[/dev/mapper/braid-ccc].flush_io_errs    1
[/dev/mapper/braid-ccc].corruption_errs  0
[/dev/mapper/braid-ccc].generation_errs  0
```
