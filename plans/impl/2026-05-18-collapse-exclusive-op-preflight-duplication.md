# Plan: collapse exclusive-op preflight duplication

## Context

`cli/src/preflight.rs` carries two near-identical sysfs walkers:

- `check_no_exclusive_op(fs, fsid)` (preflight.rs:183-194) -- single-fsid keyed read.
- `check_any_btrfs_exclusive_op(fs)` (preflight.rs:227-254) -- host-wide enumerate, with a documented `features`/`debug` skip and an empty-listing fail-closed guard.

Both build `/sys/fs/btrfs/<id>/exclusive_operation`, read it, trim, dispatch through `ExclusiveOp::parse` (preflight.rs:85-102), and wrap any read failure as `ExclusiveOpError::Read`. The duplication is small (~6 lines of glue) but it surfaces twice: two doc blocks, two test groups, two places to keep in sync if the parser contract evolves.

Worse, the only internal caller of `check_no_exclusive_op` -- `check_exclusive_op_with_policy` (preflight.rs:156-176) -- uses `Err(ExclusiveOpError::Busy(op))` as a *success* carrier for "pool is busy, here is which op," pattern-matching on an error variant for the policy-decides-what-to-do happy path.

The right shape is a private helper that returns `Result<Option<ExclusiveOp>, ExclusiveOpError>` with the natural semantics (`None` = idle, `Some(op)` = busy, `Err` = unreadable/unrecognized). Route both callers through it; delete `check_no_exclusive_op`; simplify `check_exclusive_op_with_policy` so the busy/idle split is no longer carried by an error variant; prune the redundant per-variant tests on the deleted wrapper.

Behavior-preserving cleanup. No public/crate signature changes outside the deleted private wrapper. `ExclusiveOpError::Busy` stays (it remains the return contract of `check_any_btrfs_exclusive_op`, which `idle.rs:64` consumes).

## Files modified

- `cli/src/preflight.rs` -- only file touched.

Untouched (by design):
- `cli/src/idle.rs` -- `check_any_btrfs_exclusive_op` signature preserved.
- `cli/src/lock.rs` -- `require_lock_preflight` signature preserved.
- `ExclusiveOp`, `ExclusiveOpError`, `ExclusiveOpPolicy`, `require_lock_preflight`, `require_mutation_preflight` -- all preserved.

## Changes

### 1. Add private helper `read_exclop_for_fsid`

Insert after `ExclusiveOpError` (around preflight.rs:126):

```rust
/// Read `/sys/fs/btrfs/{fsid}/exclusive_operation` and classify the
/// kernel-reported state: `Ok(None)` = idle, `Ok(Some(op))` = active
/// exclusive op, `Err` = unreadable or kernel-unknown value. Single source
/// of truth for the path format + read + trim + parse-dispatch step that
/// both the policy preflight and the multi-fsid scanner share.
fn read_exclop_for_fsid<F: Filesystem + ?Sized>(
    fs: &F,
    fsid: &str,
) -> Result<Option<ExclusiveOp>, ExclusiveOpError> {
    let path = format!("/sys/fs/btrfs/{fsid}/exclusive_operation");
    let contents = fs.read_to_string(&path).map_err(ExclusiveOpError::Read)?;
    ExclusiveOp::parse(contents.trim()).map_err(ExclusiveOpError::Unrecognized)
}
```

### 2. Delete `check_no_exclusive_op`

Remove preflight.rs:178-194 (doc + function). No external callers exist (confirmed by crate-wide grep).

### 3. Route `check_exclusive_op_with_policy` through the helper

Replace the body at preflight.rs:156-176 with:

```rust
fn check_exclusive_op_with_policy<F: Filesystem + ?Sized>(
    fs: &F,
    fsid: &str,
    policy: ExclusiveOpPolicy,
) -> Result<Option<ExclusiveOp>, String> {
    let op = match read_exclop_for_fsid(fs, fsid).map_err(|e| e.to_string())? {
        None => return Ok(None),
        Some(op) => op,
    };
    match policy {
        ExclusiveOpPolicy::RejectAnyBusy => Err(format!(
            "cannot lock: {op} is in progress. Wait for it to finish first."
        )),
        ExclusiveOpPolicy::RejectPausedBalanceElseEnqueue => match op {
            ExclusiveOp::BalancePaused => {
                Err("a btrfs balance is paused. Resume or cancel it before proceeding.".into())
            }
            _ => Ok(Some(op)),
        },
    }
}
```

No more "pattern-match `Err(Busy(op))` to extract success state." The doc comment above the function (preflight.rs:149-155) stays.

### 4. Refactor `check_any_btrfs_exclusive_op` to use the helper

Replace the inner read+parse block at preflight.rs:227-254 with:

```rust
pub(crate) fn check_any_btrfs_exclusive_op<F: Filesystem + ?Sized>(
    fs: &F,
) -> Result<(), ExclusiveOpError> {
    let entries = fs
        .list_dir("/sys/fs/btrfs")
        .map_err(ExclusiveOpError::Read)?;
    let mut found_fsid_dir = false;
    for entry in entries {
        if BTRFS_SYSFS_NON_FSID_ENTRIES.contains(&entry.as_str()) {
            continue;
        }
        found_fsid_dir = true;
        if let Some(op) = read_exclop_for_fsid(fs, &entry)? {
            return Err(ExclusiveOpError::Busy(op));
        }
    }
    if !found_fsid_dir {
        return Err(ExclusiveOpError::Read(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no btrfs filesystem found in /sys/fs/btrfs",
        )));
    }
    Ok(())
}
```

Doc comment above (preflight.rs:207-226) stays. `found_fsid_dir` flips before the read; if the read fails, `?` propagates `Err(Read)` immediately, so the empty-listing guard cannot misfire on a partial scan (same outcome as current code, just expressed more directly).

### 5. Prune redundant single-fsid tests

Delete from the "`check_no_exclusive_op` tests" group (preflight.rs:673-747) and remove the `// --- check_no_exclusive_op tests ---` section header:

- `exclusive_op_passes_when_none` -- duplicate of `exclusive_op_parse_all_variants` "none" assertion; the wrapper happy path is covered by `lock_preflight_passes_when_none` and `mutation_preflight_passes_when_none`.
- `exclusive_op_busy_when_balance_running` -- duplicate of `_parse_all_variants` "balance"; wrapper covered by `lock_preflight_rejects_busy_op`.
- `exclusive_op_busy_when_balance_paused` -- duplicate of `_parse_all_variants` "balance paused"; wrapper covered by `lock_preflight_rejects_balance_paused` + `mutation_preflight_rejects_balance_paused`.
- `exclusive_op_busy_when_device_remove` -- duplicate of `_parse_all_variants` "device remove"; non-paused-busy wrapper path covered by `lock_preflight_rejects_busy_op` and `mutation_preflight_busy_op_returns_info_note`.
- `exclusive_op_unrecognized_value` -- the parser layer is covered by `exclusive_op_parse_unrecognized`, but the parse-error -> `ExclusiveOpError::Unrecognized` -> caller-facing string wiring is not. Replaced by a new boundary test below.
- `exclusive_op_read_failure` -- the only test in this group with unique value (IO-error wrapping). Replaced by a new boundary test below.

### 6. Add boundary tests for the non-`Busy` error variants

In the `require_lock_preflight tests` group, after `lock_preflight_rejects_balance_paused` (preflight.rs:1357), add two tests that pin the user-visible wiring of `ExclusiveOpError::Read` and `ExclusiveOpError::Unrecognized` through the policy boundary -- coverage that previously lived on the deleted `check_no_exclusive_op` and is not provided by the `ExclusiveOp::parse` unit tests alone (the parser staying fail-closed does not by itself prove the wrapper surfaces a recognizable error to callers).

```rust
#[test]
// Intent: require_lock_preflight rejects when sysfs is unreadable.
// Why: Fail-closed -- if we cannot determine kernel state, lock teardown
//   must not proceed and risk unmounting mid exclusive-op.
// Scenario: /sys/fs/btrfs/{fsid}/exclusive_operation cannot be read
//   (e.g. namespace/sandbox without sysfs, kernel-level permission denied).
fn lock_preflight_rejects_on_sysfs_read_failure() {
    let fs = MockFs::empty();
    let err = require_lock_preflight(&fs, FSID).unwrap_err();
    assert!(
        err.contains("cannot read exclusive operation status"),
        "expected read-failure error, got: {err}"
    );
}

#[test]
// Intent: require_lock_preflight rejects when sysfs reports a value the
//   parser does not recognize.
// Why: Fail-closed -- a future kernel that adds a new exclop name must not
//   silently allow lock teardown. Pins the parser-error -> caller-facing
//   string wiring at the boundary that actually matters for callers; the
//   parser-layer test (`exclusive_op_parse_unrecognized`) does not.
// Scenario: New btrfs version writes a value not in exclop_def[].
fn lock_preflight_rejects_on_unrecognized_value() {
    let fs = MockFs::with_sysfs(FSID, "brand new op\n");
    let err = require_lock_preflight(&fs, FSID).unwrap_err();
    assert!(
        err.contains("unrecognized exclusive operation"),
        "expected unrecognized-value error, got: {err}"
    );
}
```

This preserves the IO-error wrap coverage previously asserted by `exclusive_op_read_failure` and the parse-error wrap coverage previously asserted by `exclusive_op_unrecognized_value`, at the public boundary that actually matters for callers, and pins the user-visible Display strings for `ExclusiveOpError::Read` and `ExclusiveOpError::Unrecognized`.

## Verification

1. `just test-rust` -- runs the affected preflight unit tests + the unchanged `idle.rs` unit tests. Expect all to pass.
   - Existing `idle.rs` tests (idle.rs:472-594) exercise `check_any_btrfs_exclusive_op` through `cmd_idle` and stay green because the signature and behavior are preserved.
   - Existing `lock_preflight_*` and `mutation_preflight_*` tests (preflight.rs:1322-1500ish) stay green because `check_exclusive_op_with_policy` produces identical output for every input.
   - New `lock_preflight_rejects_on_sysfs_read_failure` and `lock_preflight_rejects_on_unrecognized_value` pass.
2. `cargo check` (implicit in `just test-rust`) -- confirms no signature breakage anywhere in the crate.
3. Optional spot check: `just test-vm lock` to confirm end-to-end lock behavior matches. Not strictly required for a behavior-preserving refactor on a fully unit-tested module, but cheap insurance.

## Reused / existing functions

- `ExclusiveOp::parse` (preflight.rs:85-102) -- the actual contract; both walkers were already going through it. The new helper continues to.
- `ExclusiveOpError` (preflight.rs:118-126) -- all three variants stay used. `Busy` remains the return shape of `check_any_btrfs_exclusive_op` for `idle.rs`.
- `Filesystem` trait (probe.rs) -- the existing abstraction; no new methods needed.
- `MockFs::with_sysfs` and `MockFs::empty` (preflight.rs:540-602) -- the test helpers already construct exactly the shape `read_exclop_for_fsid` reads.

## Net diff estimate

~30 lines deleted (`check_no_exclusive_op` + its doc + 6 redundant tests + section header), ~35 lines added (helper + doc + simplified `check_exclusive_op_with_policy` + simplified inner loop in `check_any_btrfs_exclusive_op` + two new boundary tests). Near-zero line delta; no public API change.
