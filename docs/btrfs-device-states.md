# btrfs Device States

Status: **Draft**

When a physical drive disappears from a btrfs pool (hot-unplug, cable failure, drive death), the system passes through several states depending on how far the failure has progressed and whether the LUKS mapper is still open. Each state produces different output from `btrfs filesystem show`, `btrfs device stats`, and `cryptsetup status` — and braid must handle each combination correctly.

This document maps real-world device states to tool output and braid's internal representation. It exists because this mapping is not derivable from reading braid's code or btrfs docs alone — it requires cross-tool knowledge that's easy to get wrong.

## State Table

| State | `btrfs filesystem show` | `btrfs device stats` | `cryptsetup status` | braid maps to |
|---|---|---|---|---|
| **Healthy** | `path /dev/mapper/X` | `[/dev/mapper/X]` | `device: /dev/sdY` | `pool.devices` |
| **Null-underlying** | `path /dev/mapper/X` | `[/dev/mapper/X]` | `device: (null)` | `pool.null_underlying` |
| **MISSING with path** | `path /dev/mapper/X MISSING` | `[/dev/mapper/X]` (??) | not queried | `missing_devids` only — **gap, see below** |
| **Fully gone** | `path MISSING` | `[<missing disk>]` | not queried | `missing_devids` |

### Healthy

Normal operation. Physical drive is present, LUKS mapper is open and points to the underlying block device, btrfs sees the device.

### Null-underlying

Hot-unplug while mounted. The LUKS mapper (`/dev/mapper/braid-X`) is still open in device-mapper, but the backing block device has vanished. `cryptsetup status` reports `device: (null)`. btrfs still sees the mapper path — it doesn't know the physical drive is gone until I/O fails.

braid handles this correctly: `probe_pool` detects the `(null)` device, records it in `pool.null_underlying`, and `monitor` includes it in both the devid map and `alert_missing_devids`.

### MISSING with path

btrfs has registered the device as missing, but still remembers which mapper path it had. `btrfs filesystem show` appends `MISSING` to the path. The parser puts the devid into `missing_devids` but discards the path. `probe_pool` never processes this device (it only iterates `show.devices`), so it doesn't appear in `pool.devices` or `pool.null_underlying`.

**Gap:** If `btrfs device stats` still reports the device by its mapper path (not `<missing disk>`), the path won't be in the devid map, causing `UnmappedDeviceError` → `ComputationError` instead of a clean `MissingDevice` alert.

**Uncertainty:** We haven't empirically confirmed what `btrfs device stats` reports for a device in this state. It might report `[/dev/mapper/X]` or `[<missing disk>]`. The `??` in the table marks this. Verifying this on real hardware would close the question.

### Fully gone

Device is completely absent — either the LUKS mapper was torn down, or the device was missing at mount time (degraded mount). `btrfs filesystem show` reports bare `path MISSING` (no mapper path). `btrfs device stats` reports `[<missing disk>]`. Both parsers handle this correctly.

## Transitions

The typical progression for a hot-unplug:

```
Healthy → Null-underlying → MISSING with path(?) → Fully gone
```

The transitions depend on timing, I/O activity, and whether the kernel tears down the LUKS mapper. A brief unplug-replug might only reach Null-underlying before recovering. A permanent removal eventually reaches Fully gone.

The transition from Null-underlying to MISSING with path is the least understood. It likely happens when btrfs attempts I/O on the device and gets errors, then marks it missing — but the mapper path is still in kernel memory so btrfs remembers it.

## Code Pointers

- `probe_pool`: `cli/src/probe.rs` — builds `pool.devices`, `pool.null_underlying`, `pool.missing_devids`
- `btrfs filesystem show` parser: `cli/src/parse/btrfs_filesystem_show.rs` — filters MISSING devices from `devices` list
- `btrfs device stats` parser: `cli/src/parse/btrfs_device_stats.rs` — converts `<missing disk>` to `MissingDisk` sentinel
- monitor devid map: `cli/src/monitor.rs:61-70` — built from `pool.devices ∪ pool.null_underlying`
- alert computation: `cli/src/alert.rs:95-138` — `UnmappedDeviceError` when path not in devid map
