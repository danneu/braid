# Block `enroll` in recovery mode

## Context

When a pending-operation journal exists (`pending-op.json`), braid enters recovery mode — `pool.json` may not reflect the actual live pool. `enroll` reads `pool.json` membership to discover which disks to operate on, so running it during recovery could miss disks or target stale ones. It's also a LUKS header mutation, violating the "no mutations until recovery completes" invariant.

Currently `add`, `remove`, `replace`, `remove_missing`, and `unlock` all call `preflight::check_no_pending_operation()` — but `enroll` does not.

## Change

Add `preflight::check_no_pending_operation(paths)` at the top of `cmd_enroll_key_file`, before any other logic.

**File:** `cli/src/enroll_key_file.rs`

- Line 196: After the opening `{` of `cmd_enroll_key_file`, add:
  ```rust
  preflight::check_no_pending_operation(paths).map_err(EnrollKeyFileError::Validation)?;
  ```
- Add `use crate::preflight;` to the imports at the top of the file.

This matches the exact pattern used in `unlock.rs:30`.

## Regression test

Add a test to `cli/src/enroll_key_file.rs` (in the `mod tests` block, after the last test) that calls `cmd_enroll_key_file` with a pending-op journal present and asserts it fails before any mutation.

```rust
/*
 * Intent: enroll is blocked when a pending-operation journal exists.
 * Why: enroll reads pool.json membership to discover disks — if membership
 *   is inconsistent (mid-recovery), it could miss disks or target stale ones.
 * Scenario: an add was interrupted; pending-op.json exists. User runs
 *   braid enroll before braid recover.
 */
#[test]
fn cmd_enroll_blocked_in_recovery_mode() {
    let tmp = tempfile::TempDir::new().unwrap();
    let paths = StatePaths::custom(tmp.path().into());

    // Create a pending-op journal
    let journal = crate::journal::build_journal(
        crate::membership::PoolMembership::empty(),
        crate::membership::PoolMembership::empty(),
        crate::journal::OpKind::Add {
            disks: std::collections::BTreeMap::new(),
        },
    );
    crate::journal::write_journal(&paths, &journal).unwrap();

    // No mock commands — if enroll reaches cryptsetup, MockRunner will panic
    let runner = MockRunner::default();
    let fs = MockFs::new(&[]);
    let membership = make_membership(&[("d1", "/dev/disk/by-id/d1")]);
    let kf = tmp.path().join("braid.key");

    let err = cmd_enroll_key_file(
        &runner, &fs, &membership, &kf,
        false, false, None, &paths,
    )
    .unwrap_err();

    assert!(
        err.to_string().contains("interrupted operation"),
        "expected 'interrupted operation' in: {err}"
    );
}
```

Key design choices:
- Uses `MockRunner::default()` with no registered commands — if the preflight guard is ever removed, the test fails with a panic from MockRunner (unrecognized command), not a silent pass.
- Tests the `cmd_enroll_key_file` entry point directly, not a sub-function.
- Follows the existing test comment convention (Intent/Why/Scenario).

## Verification

`just test-rust` — runs the new regression test plus all existing tests.
