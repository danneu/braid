---
intent: Map every device-disappearance state to the output of btrfs, cryptsetup, and the kernel, and show how braid maps each combination to internal types. Read this before modifying probe, monitor, or alert code.
status: Draft
---

# Device Disappearance States

When a physical drive disappears from a btrfs pool (hot-unplug, cable failure, drive death), the system passes through several states depending on how far the failure has progressed and whether the LUKS mapper is still open. Each state produces different output from `btrfs filesystem show`, `btrfs device stats`, and `cryptsetup status` — and braid must handle each combination correctly.

This mapping is not derivable from reading braid's code or btrfs docs alone — it requires cross-tool knowledge that's easy to get wrong.

## State Table

| State                 | `btrfs filesystem show`      | `btrfs device stats`   | `cryptsetup status` | braid maps to          |
| --------------------- | ---------------------------- | ---------------------- | ------------------- | ---------------------- |
| **Healthy**           | `path /dev/mapper/X`         | `[/dev/mapper/X]`      | `device: /dev/sdY`  | `pool.devices`         |
| **Null-underlying**   | `path /dev/mapper/X`         | `[/dev/mapper/X]`      | `device: (null)`    | `pool.null_underlying` |
| **MISSING with path** | `path /dev/mapper/X MISSING` | `[/dev/mapper/X]` (??) | not queried         | `missing_devids` only  |
| **Fully gone**        | `path MISSING`               | `[devid:N]`            | not queried         | `missing_devids`       |

**Empirical note**: SATA hot-unplug on real hardware enters Null-underlying immediately and stays there for at least 5 minutes without I/O pressure. We have not yet observed the MISSING-with-path state in practice. See [`real-world/sata-hot-unplug.md`](../real-world/sata-hot-unplug.md) for full test results.

### Healthy

Normal operation. Physical drive is present, LUKS mapper is open and points to the underlying block device, btrfs sees the device.

### Null-underlying

Hot-unplug while mounted. The LUKS mapper (`/dev/mapper/braid-X`) is still open in device-mapper, but the backing block device has vanished. `cryptsetup status` reports `device: (null)`. btrfs still sees the mapper path — it doesn't know the physical drive is gone until I/O fails.

braid handles this correctly: `probe_pool` detects the `(null)` device, records it in `pool.null_underlying`, and `monitor` includes its devid in `alert_missing_devids`. The stats row reports both the mapper path and the devid; the alert pipeline pairs by devid directly.

Post-UUID-identity rule: when a mapper is null-underlying, the live LUKS UUID is
not observable from the missing backing device. braid may bind that live mapper
back to membership through persisted `DiskMember.devid`, but only for this
restricted case. The persisted devid is prior-binding state, not display
authority; status output still uses live btrfs stats for displayed devids.

### MISSING with path

btrfs has registered the device as missing, but still remembers which mapper path it had. `btrfs filesystem show` appends `MISSING` to the path. The parser puts the devid into `missing_devids` but discards the path. `probe_pool` never processes this device (it only iterates `show.devices`), so it doesn't appear in `pool.devices` or `pool.null_underlying`.

**Handling:** `btrfs device stats` rows always carry a mandatory `devid` field, so the alert pipeline identifies the row by devid regardless of which path string btrfs reports (`[/dev/mapper/X]` or `[devid:N]`). The `MissingDevice` alert is generated independently from `missing_devids`. Rows for alert-local missing devids are skipped for `BtrfsDeviceErrors`, while `braid ack` still snapshots their counters by devid so old counts do not re-alert if the member returns.

The same restricted devid fallback applies to membership correlation: when
btrfs reports a missing device only by devid, braid can resolve the member whose
persisted `DiskMember.devid` matches. It must not infer membership by parsing a
mapper name or LUKS label.

**Uncertainty:** We haven't empirically confirmed which path string `btrfs device stats` reports for a device in this state -- the `??` in the table marks this. The answer no longer affects correctness (devid drives the lookup), but it would still be useful empirical data.

### Fully gone

Device is completely absent — either the LUKS mapper was torn down, or the device was missing at mount time (degraded mount). `btrfs filesystem show` reports bare `path MISSING` (no mapper path). Pinned btrfs-progs v6.17.1 renders the missing-device stats path as `[devid:N]` (`device.c:625-634`); `[<missing disk>]` is an older btrfs rendering. braid does not depend on either string: the parser ignores the device field and keeps the row's `devid` and counters.

At this point there is no mapper and no observable LUKS UUID. Mutating commands
that target the missing device, such as `remove-missing` and missing-path
`replace`, resolve the requested btrfs devid through UUID-keyed membership and
fail closed if no persisted member carries that devid.

## Transitions

The typical progression for a hot-unplug:

```
Healthy → Null-underlying → MISSING with path(?) → Fully gone
```

The transitions depend on timing, I/O activity, and whether the kernel tears down the LUKS mapper. A brief unplug-replug might only reach Null-underlying before recovering. A permanent removal eventually reaches Fully gone.

The transition from Null-underlying to MISSING with path is the least understood. It likely happens when btrfs attempts I/O on the device and gets errors, then marks it missing — but the mapper path is still in kernel memory so btrfs remembers it.

## Code Pointers

- `probe_pool`: `cli/src/probe.rs` -- builds `pool.devices`, `pool.null_underlying`, `pool.missing_devids`
- `btrfs filesystem show` parser: `cli/src/parse/btrfs_filesystem_show.rs` -- filters MISSING devices from `devices` list
- `btrfs device stats` parser: `cli/src/parse/btrfs_device_stats.rs` -- propagates `devid` as the btrfs-native stats row key and ignores the display-only device string
- alert computation: `cli/src/alert.rs` -- `compute_alert_state` and `snapshot_current` key by `dev.devid` from the parsed stats row; `compute_alert_state` skips alert-local missing devids for `BtrfsDeviceErrors`
