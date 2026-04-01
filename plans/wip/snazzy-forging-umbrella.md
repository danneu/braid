# Plan: Post-recovery guidance messages

## Context

After crash recovery, `braid recover` prints three raw membership sets (pre, target, recovered) but never compares them or tells the user what happened. A user unfamiliar with braid internals has to manually diff the sets to figure out whether the interrupted operation completed, rolled back, or partially applied — and whether they need to re-run anything.

## Approach

Add a pure helper function `recovery_guidance()` in `cli/src/recover.rs` that takes the journal and the three name sets, returns a `String` with a one-sentence guidance message. Call it from `cmd_recover` after the set printout (line 152). Unit test it directly for all op types and outcomes.

## File to modify

`cli/src/recover.rs`

## Helper signature

```rust
fn recovery_guidance(
    op: &OpKind,
    pre_names: &BTreeSet<&String>,
    target_names: &BTreeSet<&String>,
    recovered_names: &BTreeSet<&String>,
) -> String
```

## Logic

Three outcome branches, then per-op messages:

### `recovered == target` (operation completed)

| Op | Message (exact) |
|---|---|
| `Add { disks }` | `"add completed — 'disk3' now in the pool."` (join with `", "` if multiple: `"'disk3', 'disk4'"`) |
| `Remove { name }` | `"remove completed — 'toshiba' is no longer in the pool."` |
| `RemoveMissing { .. }` | `"remove-missing completed — missing device removed from the pool."` |
| `Replace { old_name, new_name, .. }` | `"replace completed — 'old' replaced by 'new'."` |

### `recovered == pre` (operation did not complete)

| Op | Message (exact) |
|---|---|
| `Add { disks }` | `"add did not complete — 'disk3' not in the pool. Re-run braid add to retry."` |
| `Remove { name }` | `"remove did not complete — 'toshiba' is still in the pool. Re-run braid remove to retry."` |
| `RemoveMissing { .. }` | `"remove-missing did not complete — device still in the pool. Re-run braid remove-missing to retry."` |
| `Replace { old_name, new_name, .. }` | `"replace did not complete — pool still has 'old'. Re-run braid replace to retry."` |

### Otherwise (partial / unexpected)

Generic (exact): `"pool membership does not match the pre-operation or target state. Run braid status and decide whether to re-run the operation."`

Note: `RemoveMissing` uses a generic "missing device" / "device" phrasing since the `OpKind` only carries an optional `devid`, not a disk name.

## Call site

After line 152 in `cmd_recover`:

```rust
eprintln!(
    "note: {}",
    recovery_guidance(&journal.op, &pre_names, &target_names, &recovered_names)
);
```

## Unit tests

Add tests in the existing `#[cfg(test)] mod tests` block. These test the pure helper directly — no mocks needed. Every assertion uses `assert_eq!` against the **exact expected string**.

| Test name | Op | Outcome | Expected string |
|---|---|---|---|
| `guidance_add_completed` | `Add { "disk3" }` | `recovered == target` | `"add completed — 'disk3' now in the pool."` |
| `guidance_add_rolled_back` | `Add { "disk3" }` | `recovered == pre` | `"add did not complete — 'disk3' not in the pool. Re-run braid add to retry."` |
| `guidance_remove_completed` | `Remove { "toshiba" }` | `recovered == target` | `"remove completed — 'toshiba' is no longer in the pool."` |
| `guidance_remove_rolled_back` | `Remove { "toshiba" }` | `recovered == pre` | `"remove did not complete — 'toshiba' is still in the pool. Re-run braid remove to retry."` |
| `guidance_remove_missing_completed` | `RemoveMissing { devid: Some(2) }` | `recovered == target` | `"remove-missing completed — missing device removed from the pool."` |
| `guidance_remove_missing_rolled_back` | `RemoveMissing { devid: Some(2) }` | `recovered == pre` | `"remove-missing did not complete — device still in the pool. Re-run braid remove-missing to retry."` |
| `guidance_replace_completed` | `Replace { old_name: "old", new_name: "new", new_by_id: .. }` | `recovered == target` | `"replace completed — 'old' replaced by 'new'."` |
| `guidance_replace_rolled_back` | `Replace { old_name: "old", new_name: "new", new_by_id: .. }` | `recovered == pre` | `"replace did not complete — pool still has 'old'. Re-run braid replace to retry."` |
| `guidance_partial` | `Add { "disk3" }` | neither | `"pool membership does not match the pre-operation or target state. Run braid status and decide whether to re-run the operation."` |

9 tests total, all exact-match.

## Verification

1. `just test-rust` — run Rust unit tests including the new helper tests.
2. `just test` — run VM tests to confirm recovery flow isn't broken.
