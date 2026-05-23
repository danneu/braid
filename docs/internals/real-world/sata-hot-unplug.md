---
intent: Record what btrfs, cryptsetup, the kernel, and udev actually report during SATA hot-unplug and replug on real hardware. Validates the state model in tool-behavior/device-disappearance.md and the detection-signal latencies that braid relies on.
status: Active
---

# SATA Hot-Unplug and Replug Behavior

Empirical observations from physical hardware testing. Validates the device state model in [`tool-behavior/device-disappearance.md`](../tool-behavior/device-disappearance.md).

## Hardware

- Machine: Silverstone NAS (hunk)
- Drives: 3x SATA HDD in btrfs RAID1 over LUKS
- Disk removed: ccc (ST500LM021, devid 3, `wwn-0x5000c500ba0a8b52`, LUKS label `braid-ccc`)
- OS: NixOS with braid module

## Detection signals and latencies

How fast each layer notices the disk is gone, and what passive signals are available without user-initiated I/O.

| Signal                                     | Latency                   | Passive?    | Programmatic detection          |
| ------------------------------------------ | ------------------------- | ----------- | ------------------------------- |
| `ata*: SATA link down` (kernel journal)    | **Instant**               | Yes         | `journalctl -kf` pattern match  |
| udev `remove` event                        | ~11s (after SATA retries) | Yes         | udev rule on `ACTION=="remove"` |
| `/dev/disk/by-id/wwn-*` symlink disappears | ~11s (udev cleans it)     | Yes         | inotify on `/dev/disk/by-id/`   |
| `cryptsetup status` shows `device: (null)` | ~11s                      | Yes         | poll `cryptsetup status`        |
| btrfs write errors (periodic commit)       | ~26s                      | Yes         | `journalctl -kf` pattern match  |
| `btrfs device stats` shows nonzero errors  | ~26s+                     | Needs query | `btrfs device stats`            |

Key takeaway: the kernel journal and udev events are the fastest passive signals. btrfs is completely oblivious until its next periodic commit (~30s default), but then notices on its own without user-initiated I/O.

The udev remove event is especially useful -- it includes `ID_WWN` and `ID_FS_LABEL` (e.g. `braid-ccc`), so a udev rule can immediately identify which braid disk disappeared.

### What does NOT react

- **LUKS mapper** (`/dev/mapper/braid-ccc`): stays as a zombie. `cryptsetup status` still says "active" but the backing `device:` becomes `(null)`. I/O through it fails.
- **`btrfs filesystem show`**: continues to list all 3 devices with paths and sizes even after errors. Never reports the device as missing from this command alone.

### udev remove event (raw)

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

## Test: SATA Hot-Unplug (disk removed while pool mounted)

### Immediate state (seconds after unplug)

| Tool                          | Output                                                       |
| ----------------------------- | ------------------------------------------------------------ |
| `btrfs filesystem show`       | Still lists `path /dev/mapper/braid-ccc` — no MISSING suffix |
| `btrfs device stats`          | Still lists `[/dev/mapper/braid-ccc]` — not `<missing disk>` |
| `cryptsetup status braid-ccc` | `active and is in use`, `device: (null)`                     |
| `braid status`                | DEGRADED, ccc = missing                                      |
| `braid monitor`               | Exit 1 (alert), clean `MissingDevice { devid: 3 }`           |

**Conclusion**: Immediate hot-unplug enters the **null-underlying** state. btrfs doesn't know the device is gone — it still reports the mapper path. Only cryptsetup detects the loss (underlying block device vanished). braid's null-underlying detection handles this correctly.

### State after ~5 minutes (still unplugged)

No change. `btrfs filesystem show` still reports the path without MISSING. btrfs doesn't transition to the MISSING state on its own without I/O pressure. The null-underlying state is stable for at least minutes.

### Kernel perspective (dmesg)

```
[ 3431s] ata1: SATA link down (SStatus 0 SControl 300)
[ 3437s] ata1: SATA link down — limiting SATA link speed
[ 3442s] ata1.00: disable device, detaching (SCSI 0:0:0:0)
[ 3442s] sd 0:0:0:0: [sdc] Synchronize Cache failed: DID_BAD_TARGET
```

Kernel detects the link-down within seconds and detaches the SCSI device. The LUKS mapper (`dm-2`) stays open — dm-crypt doesn't tear down when the underlying device vanishes.

## Test: SATA Replug (disk reconnected)

### State after replug

| Tool                          | Output                                               |
| ----------------------------- | ---------------------------------------------------- |
| `btrfs filesystem show`       | Still lists `path /dev/mapper/braid-ccc` (unchanged) |
| `btrfs device stats`          | Still lists `[/dev/mapper/braid-ccc]` (unchanged)    |
| `cryptsetup status braid-ccc` | **Still `device: (null)`** — does NOT recover        |
| `braid status`                | ccc still shows as missing / UNKNOWN                 |
| Physical device               | Back as `/dev/sde` (was `/dev/sdc` before unplug)    |

**Key finding**: The LUKS mapper does not recover from null-underlying after replug. The dm-crypt target was `/dev/sdc`, but the kernel re-attached the disk as `/dev/sde`. The mapper is permanently broken until closed and reopened.

### Kernel perspective (dmesg)

```
[ 3744s] ata1: SATA link up 6.0 Gbps (SStatus 133 SControl 300)
[ 3744s] ata1.00: ATA-8: ST500LM021-1KJ152
[ 3744s] sd 0:0:0:0: [sde] 976773168 512-byte logical blocks
[ 3744s] sd 0:0:0:0: [sde] Attached SCSI disk
```

Kernel sees the disk on the same ATA port but assigns a new SCSI device node (`sde` instead of `sdc`).

### Recovery path

The broken LUKS mapper cannot self-heal. Recovery requires:

1. `braid ack` to silence the alert
2. Reboot → `braid unlock` (reopens LUKS mappers using stable `/dev/disk/by-id/` paths)

This is correct behavior — braid uses by-id paths for LUKS open, so a reboot always rebinds to the right device regardless of kernel device node assignment.

## Unanswered Questions

- **MISSING-with-path state**: We never observed `btrfs filesystem show` report `path /dev/mapper/X MISSING` during these tests. This state may require sustained I/O errors or a degraded mount (reboot with disk missing). The `??` in the device state table for what `btrfs device stats` reports in this state remains unverified.
- **Time to MISSING transition**: btrfs didn't transition from null-underlying to MISSING within 5 minutes of idle. It may require write pressure or a longer timeout.
- **Replug with same device node**: We didn't test whether cryptsetup recovers if the kernel assigns the same `/dev/sdX` path after replug. Unlikely in practice since the kernel increments device letters.

## Validated Code Paths

Changes to these should prompt re-verification of this document:

- `cli/src/probe.rs` -- `probe_pool()` null-underlying detection (lines 190-206)
- `cli/src/monitor.rs` -- alert-local missing devids union (`missing_devids ∪ null_underlying` devids)
- `cli/src/alert.rs` -- `compute_alert_state` / `snapshot_current` (devid-keyed; no path-to-devid map)
- `cli/src/parse/btrfs_filesystem_show.rs` -- MISSING device filtering (line 116)
- `cli/src/parse/btrfs_device_stats.rs` -- `devid` propagation and `<missing disk>` / `devid:<n>` sentinel handling
