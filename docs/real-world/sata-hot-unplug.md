---
intent: Record what btrfs, cryptsetup, and the kernel actually report during SATA hot-unplug and replug on real hardware. Validates the state model in tool-behavior/device-disappearance.md.
---

# SATA Hot-Unplug and Replug Behavior

Empirical observations from physical hardware testing. Validates the device state model in [`tool-behavior/device-disappearance.md`](../tool-behavior/device-disappearance.md).

## Hardware

- Machine: Silverstone NAS (hunk)
- Drives: 3x SATA HDD in btrfs RAID1 over LUKS
- Disk removed: ccc (ST500LM021, devid 3)
- OS: NixOS with braid module

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
