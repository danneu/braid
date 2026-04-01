# Fix: braid monitor crash on hot-unplugged disk

## Context

When a physical drive is hot-unplugged from a live braid pool, `braid monitor` exits 2 (error) instead of exit 1 (alert). The systemd wrapper (`modules/braid/monitor.nix:91-99`) only starts `braid-alert.service` on exit 1 — exit 2 is treated as a monitor failure. **No beep fires**, breaking the core alerting promise for the most common disk failure scenario.

Additionally, `braid ack` crashes with the same error, making it impossible to silence the alert once btrfs eventually catches up.

### What happens on hot-unplug

1. Underlying `/dev/sdX` vanishes
2. LUKS dm device `/dev/mapper/braid-ccc` stays open (dm doesn't auto-close)
3. `cryptsetup status braid-ccc` reports `device: (null)`
4. btrfs continues reporting the mapper path in both `filesystem show` and `device stats`

### Bug 1 — devid map gap (`monitor.rs:54-58`, `ack.rs:42-46`)

The devid map (`BTreeMap<String, u64>`) is built from `pool.devices`, which only contains devices that passed the cryptsetup probe. `probe_pool` (`probe.rs:187-190`) skips `(null)` devices with `continue`, so the mapper→devid mapping is never recorded. But `btrfs device stats` still reports the mapper path. `compute_alert_state_with_devid_map` (`alert.rs:110-112`) and `snapshot_current` (`alert.rs:195-197`) look up the path, can't find it → `UnmappedDeviceError` → exit 2.

### Bug 2 — missing_devids gap (`probe.rs:214`)

`pool.missing_devids` comes from btrfs `MISSING` sentinels parsed from `btrfs filesystem show`. But when the LUKS mapper still exists, btrfs doesn't mark the device as MISSING — it shows the mapper path normally. The devid never enters `missing_devids`, so even if bug 1 were fixed, `MissingDevice { devid }` would never fire for this window.

### Timing

After enough failed I/O (seconds to minutes), btrfs promotes the device to fully MISSING and the existing code paths work. The bug is the window between hot-unplug and btrfs catching up, where monitor crashes instead of alerting.

---

## Fix

### 1. New struct: `NullUnderlyingDevice` (`cli/src/types.rs`)

```rust
/// A pool device whose LUKS mapper is open but the underlying block device
/// is gone (hot-unplugged). These are effectively missing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NullUnderlyingDevice {
    pub mapper: MapperName,
    pub devid: u64,
}
```

### 2. New field on `PoolState` (`cli/src/types.rs:68`)

```rust
pub struct PoolState {
    // ... existing fields ...
    /// Devices whose LUKS mapper is open but underlying block device is gone.
    /// Alert-only: NOT added to `missing_devids` (which is consumed by
    /// `remove-missing` to resolve destructive targets). Monitor and ack
    /// compute an alert-local union instead.
    pub null_underlying: Vec<NullUnderlyingDevice>,
}
```

### 3. Capture null-underlying devices in `probe_pool` (`cli/src/probe.rs:157-214`)

Instead of bare `continue` on `(null)`, push to a `null_underlying` vec. Do **not** merge into `missing_devids` — that field is consumed by `remove-missing` (`remove_missing.rs:179`) to resolve destructive targets, and a transient hot-unplug should not look like a confirmed removal candidate.

```rust
let mut devices = Vec::new();
let mut null_underlying = Vec::new();  // NEW

for bdev in &show.devices {
    // ... existing path validation, cryptsetup status ...

    let underlying = match status.device {
        None => {
            null_underlying.push(NullUnderlyingDevice {
                mapper: MapperName(name),
                devid: bdev.devid,
            });
            continue;
        }
        Some(ref d) if d == "(null)" => {
            null_underlying.push(NullUnderlyingDevice {
                mapper: MapperName(name),
                devid: bdev.devid,
            });
            continue;
        }
        Some(d) => d,
    };
    // ... rest unchanged ...
}

Ok(PoolState {
    mounted: true,
    devices,
    missing_count,
    total_devices: show.total_devices,
    fsid: Some(fsid),
    missing_devids: show.missing_devids,  // btrfs-authoritative, unchanged
    null_underlying,                       // NEW — alert-only
})
```

### 4. Include null-underlying devices in devid map — `monitor.rs:54-58`

```rust
let path_to_devid: BTreeMap<String, u64> = pool
    .devices
    .iter()
    .map(|d| (format!("/dev/mapper/{}", d.mapper.0), d.devid))
    .chain(
        pool.null_underlying
            .iter()
            .map(|d| (format!("/dev/mapper/{}", d.mapper.0), d.devid)),
    )
    .collect();
```

### 5. Compute alert-local missing devids — `monitor.rs:48` and `ack.rs:39`

Instead of passing `&pool.missing_devids` directly, compute an alert-local union:

```rust
// monitor.rs (after line 48) and ack.rs (after line 39):
let alert_missing_devids: Vec<u64> = pool
    .missing_devids
    .iter()
    .copied()
    .chain(pool.null_underlying.iter().map(|d| d.devid))
    .collect::<BTreeSet<u64>>()
    .into_iter()
    .collect();
```

De-duplication via `BTreeSet` prevents doubled `MissingDevice { devid }` causes when btrfs has promoted the device to real MISSING while the LUKS mapper still reports `(null)`.

Pass `&alert_missing_devids` to `compute_alert_state_with_devid_map` and `snapshot_current` instead of `&pool.missing_devids`. This keeps `pool.missing_devids` authoritative to btrfs-reported MISSING sentinels (safe for `remove-missing`) while ensuring null-underlying devices trigger `MissingDevice` alert causes.

### 6. Same devid map fix in `ack.rs:42-46`

Identical `.chain(pool.null_underlying...)` addition for the devid map, plus the `alert_missing_devids` union above.

### 7. Remaining callers

Exploration confirmed only two call sites build devid maps from `pool.devices`: `monitor.rs:54-58` and `ack.rs:42-46`. Both are covered above.

### 8. Initialize `null_underlying` in all other `PoolState` constructions

`probe_pool` has two return paths — mounted (the one we're fixing) and unmounted. The unmounted path and any test helpers that construct `PoolState` need `null_underlying: vec![]`.

---

## Tests

### Rust unit test: extend `probe_pool_device_null_underlying` (`cli/src/probe.rs:738`)

Add assertions to the existing test:

```rust
// Existing assertions remain...
assert_eq!(result.null_underlying.len(), 1);
assert_eq!(result.null_underlying[0].mapper, MapperName("braid-ironwolf".into()));
assert_eq!(result.null_underlying[0].devid, 2);
// missing_devids stays btrfs-authoritative — null-underlying devids are NOT injected
assert!(result.missing_devids.is_empty());
```

### Rust unit test: alert computation with null-underlying device (`cli/src/alert.rs` or `cli/src/monitor.rs`)

New test: build a devid map that includes a null-underlying device's mapper→devid entry, compute the alert-local `missing_devids` union (btrfs missing ∪ null-underlying devids), provide `btrfs device stats` output that references the mapper path. Assert:
- `compute_alert_state_with_devid_map` returns `Ok` (no `UnmappedDeviceError`)
- Result contains `AlertCause::MissingDevice { devid }` for the null-underlying device

### NixOS VM test: hot-unplug monitor lifecycle

New test files: `tests/cli/monitor-hot-unplug.nix` + `tests/cli/monitor-hot-unplug.py`

**NixOS config** (`.nix`):
- 3 virtio disks with `serial` and `id` fields (id needed for `device_del`)
- braid CLI + cryptsetup + btrfs-progs in systemPackages
- braid config.json pointing at `/mnt/storage`

**Test script** (`.py`):

```
Intent: Verify that hot-unplugging a disk from a live RAID1 pool triggers
  the alert lifecycle correctly, even during the window before btrfs marks
  the device as MISSING.

Why it exists: On real hardware, hot-unplug caused braid monitor to exit 2
  (error) instead of exit 1 (alert) because the LUKS mapper persisted with
  device: (null). No beep fired — the core alerting promise was broken.

Scenario: 3-disk RAID1 pool is mounted. QEMU ACPI hot-unplug removes one
  disk's underlying block device while the LUKS mapper stays open. braid
  monitor must detect this as a missing device, exit 1, and enable the full
  alert lifecycle including braid ack.
```

Steps:
1. Create 3-disk LUKS+btrfs RAID1 pool, mount, `mkdir /var/lib/braid`
2. Healthy: `braid monitor` exits 0
3. Hot-unplug disk2: `machine.send_monitor_command("device_del disk2dev")`
4. Wait for `cryptsetup status braid-disk2` to report `(null)` — this is the gate, not the by-id symlink disappearing. Use `machine.wait_until_succeeds("cryptsetup status braid-disk2 | grep '(null)'")`; this avoids the race between udev removing the symlink and dm updating the device status.
5. Assert LUKS mapper still exists: `machine.succeed("test -e /dev/mapper/braid-disk2")`
6. `braid monitor` exits 1 (not 2): `rc = machine.succeed("set +e; braid monitor; echo $?").strip().splitlines()[-1]`; `assert rc == "1"`
7. Alert latch created: `machine.succeed("test -f /var/lib/braid/alert-latch.json")`
8. `braid status` shows ALERT with missing device cause
9. `braid ack` exits 0
10. Alert latch removed: `machine.fail("test -f /var/lib/braid/alert-latch.json")`
11. After ack: `braid monitor` exits 0

---

## Files to modify

| File | Change |
|---|---|
| `cli/src/types.rs` | Add `NullUnderlyingDevice` struct, add `null_underlying` field to `PoolState` |
| `cli/src/probe.rs` | Capture null-underlying devices instead of bare `continue`; do NOT merge into `missing_devids` |
| `cli/src/monitor.rs` | Chain `null_underlying` into devid map; compute alert-local missing devids union |
| `cli/src/ack.rs` | Chain `null_underlying` into devid map; compute alert-local missing devids union |
| `cli/src/probe.rs` (tests) | Extend `probe_pool_device_null_underlying` with new assertions |
| `cli/src/alert.rs` (tests) | New test for alert computation with null-underlying device in devid map |
| `tests/cli/monitor-hot-unplug.nix` | New VM test config |
| `tests/cli/monitor-hot-unplug.py` | New VM test script |

Plus: any other `PoolState` construction sites that need `null_underlying: vec![]` (unmounted path in `probe_pool`, test helpers).

## Verification

1. `just test-rust` — unit tests pass, including extended and new tests
2. `just test braid-monitor` — existing monitor lifecycle test still passes
3. `just test monitor-hot-unplug` — new VM test passes (monitor exits 1, ack works, alert lifecycle complete)
