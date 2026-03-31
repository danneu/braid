# Fix: Error when remove-missing cannot resolve the missing disk identity

## Context

`cli/src/remove_missing.rs:148-163` has two paths that silently corrupt pool.json:

1. **`target_devid` is None.** Line 149: `missing_id.or_else(|| pool.missing_devids.first().copied())`. When btrfs prints only the `*** Some devices missing` sentinel (no explicit `devid N ... MISSING` line), `pool.missing_devids` is empty. `target_devid` becomes `None`, the `if let Some(devid)` block is skipped entirely, `target_membership == pre_membership`, journal records no membership change, btrfs removal succeeds, pool.json is saved with the dead disk still listed.

2. **`target_devid` is Some but no member has that devid enriched.** `find(|(_, member)| member.devid == Some(devid))` returns `None` when the `devid` field was never enriched in pool.json. Same result: pool.json saved unchanged.

Both are the same logical failure: "cannot resolve which pool.json entry corresponds to the missing device." The fix should treat this as one resolution step with one error path.

## Fix

**File:** `cli/src/remove_missing.rs`

Add a helper that resolves the removal target to a `(devid, disk_name)` pair before journaling. If resolution fails, return a `RemoveMissingError::Validation` and abort before any irreversible operation.

### Concrete changes

**1. Add resolution helper** (above `cmd_remove_missing` or as a local function):

```rust
/// Resolve the missing-device removal target to a (devid, membership-name) pair.
/// Returns Err if the missing device's identity can't be mapped to a pool.json entry.
fn resolve_removal_target(
    target_devid: Option<u64>,
    membership: &PoolMembership,
) -> Result<(u64, String), RemoveMissingError> {
    let devid = target_devid.ok_or_else(|| {
        RemoveMissingError::Validation(
            "cannot determine which device to remove: btrfs did not report \
             the missing device's ID. Pass --missing-id <devid> explicitly."
                .into(),
        )
    })?;

    let name = membership
        .disks
        .iter()
        .find(|(_, member)| member.devid == Some(devid))
        .map(|(name, _)| name.clone())
        .ok_or_else(|| {
            RemoveMissingError::Validation(format!(
                "devid {devid} not found in pool.json membership — \
                 no disk entry has this device ID. \
                 Pool membership may need manual repair."
            ))
        })?;

    Ok((devid, name))
}
```

**2. Replace lines 148-163** in `cmd_remove_missing` with:

```rust
let target_devid = missing_id.or_else(|| pool.missing_devids.first().copied());
let pre_membership = membership::load_membership(paths).map_err(|e| {
    RemoveMissingError::Validation(format!("failed to load pool membership: {e}"))
})?;
let (resolved_devid, name_to_remove) = resolve_removal_target(target_devid, &pre_membership)?;
let mut target_membership = pre_membership.clone();
target_membership.disks.remove(&name_to_remove);
```

Use `resolved_devid` (not `target_devid`) in the journal payload and the execute block — even though they're equivalent after resolution succeeds, binding from the resolved value keeps the invariant explicit:

```rust
let journal = journal::build_journal(
    pre_membership,
    target_membership.clone(),
    journal::OpKind::RemoveMissing {
        devid: Some(resolved_devid),
    },
);
```

And in the execute block, use `resolved_devid` for the `--missing-id` path and the eprintln.

### Tests

**File:** `cli/src/remove_missing.rs` (test module)

Two new unit tests for `resolve_removal_target`:

1. **`resolve_target_fails_when_devid_unavailable`** — `target_devid` is `None`. Asserts `Validation` error containing "missing device's ID".

2. **`resolve_target_fails_when_devid_not_in_membership`** — `target_devid` is `Some(99)`, membership has disks but none with `devid == Some(99)`. Asserts `Validation` error containing "not found in pool.json".

### Existing test impact

The existing `RecordingRunner` (2-device mock, line 369) returns `*** Some devices missing` without explicit devid lines, so `missing_devids` is empty and `target_devid` will be `None`. This means these tests will now hit the new error path:

- `enospc_check_skipped_for_single_survivor` (line 415)
- `three_device_pool_soft_rebalance_runs` (line 780)
- `three_device_two_missing_no_rebalance` (line 821)

These mocks need to be updated to include explicit missing-devid lines in their `BtrfsFilesystemShow` output **and** the `test_paths` helper needs to set `devid` on the `DiskMember` entries so the resolution succeeds. For example, the 2-device RecordingRunner output at line 369 should become:

```
"...\n\tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n\tdevid    2 size 0 used 0 path MISSING\n"
```

And `test_paths` entries should set devids matching the mock (disk1 → devid 1, disk2 → devid 2, etc.).

## Verification

1. `just test-rust` — all existing + new tests pass.
2. Confirm the two new tests fail without the production code change (TDD sanity check).
