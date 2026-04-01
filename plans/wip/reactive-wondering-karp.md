# Fail-closed on missing FSID in add path

## Context

`classify_braid_disk_fsid` (`cli/src/add.rs:109`) uses `if let (Some, Some)` to compare the device's btrfs UUID against the pool's FSID. If either is `None`, the comparison is silently skipped and execution falls through to the AlreadyInPool/Recoverable check. This is fail-open — a foreign-pool disk could be misclassified as recoverable and added to the wrong pool, corrupting data.

Two different values can be `None`, and each belongs to a different layer:

1. **`pool.fsid`** — populated by `probe_pool` (`cli/src/probe.rs:205`). A mounted pool always has a FSID; `None` here means `btrfs filesystem show` succeeded but the parser didn't find a `uuid:` line. This is a probe invariant violation — reject it in `probe_pool`, not downstream.

2. **`show.uuid`** (device side) — populated by `parse_btrfs_filesystem_show` on the candidate device's mapper. The device already passed `HasBtrfs` (exit 0), so a missing UUID is a parser edge case. This belongs in `classify_braid_disk_fsid`.

## Changes

### 1. `cli/src/probe.rs` — reject mounted pool with no FSID

After line 147 (`let show = parse_btrfs_filesystem_show(&show_raw)?;`), add a guard:

```rust
let fsid = show.uuid.ok_or_else(|| ProbeError::PoolDevice {
    mapper: mount_point.to_owned(),
    detail: "mounted pool has no FSID in btrfs filesystem show output".into(),
})?;
```

Then at line 205, use the validated value:

```rust
fsid: Some(fsid),
```

### 2. `cli/src/add.rs` — fail-closed on device-side UUID only

Replace lines 109-113:

```rust
if let (Some(device_fsid), Some(pool_fsid)) = (&show.uuid, &pool.fsid) {
    if device_fsid != pool_fsid {
        return Ok(AddLuksIdentity::BraidLabeledForeignPool);
    }
}
```

With:

```rust
// The device passed HasBtrfs (exit 0) so btrfs filesystem show should
// have printed a uuid line. None means the parser couldn't extract it —
// fail rather than silently skipping the foreign-pool guard.
let device_fsid = show.uuid.as_ref().ok_or_else(|| {
    AddError::Validation(format!(
        "disk '{}': btrfs superblock present but no UUID in \
         btrfs filesystem show output",
        name,
    ))
})?;

// pool.fsid is guaranteed Some for mounted pools by probe_pool.
let pool_fsid = pool.fsid.as_ref().expect("mounted pool must have FSID");

if device_fsid != pool_fsid {
    return Ok(AddLuksIdentity::BraidLabeledForeignPool);
}
```

### 3. Tests

**`cli/src/probe.rs`** — add `probe_pool_errors_on_missing_fsid`: mock `btrfs filesystem show` to return valid output with no `uuid:` line. Assert `probe_pool` returns `Err(ProbeError::PoolDevice { .. })`.

**`cli/src/add.rs`** — add `classify_fsid_errors_on_missing_device_uuid`: mock `btrfs filesystem show <target>` to return exit 0 with no `uuid:` line. Assert `classify_braid_disk_fsid` returns `Err(AddError::Validation(..))`.

## Verification

`just test-rust`
