# Plan: drop redundant `will_clear_last_missing` from `RemoveMissingWorkPlan`

## Context

`RemoveMissingWorkPlan` (private struct in `cli/src/remove_missing.rs`)
carries two fields that encode overlapping pre-op state:

- `will_clear_last_missing: bool` -- exactly `pool.missing_count == 1`
- `missing_count: u64` -- the raw count from `pool.missing_count`

The bool was introduced in commit `f60c8b3 refactor(cli): unify plan
execution around typed work plans`, which lifted these fields out of the
public `RemoveMissingPlan` into the private work plan without
consolidating them. A low-severity review finding flagged the threading
of `missing_count` as load-bearing and proposed dropping it. Verifying
the finding showed the inverse is the cleaner cut: `missing_count` is
the genuinely load-bearing field (the confirm formatter's `> 1` branches
print the literal count and `count - 1`), and `will_clear_last_missing`
is the redundant derivation.

The pivot: make `missing_count` the single source of truth for pre-op
missing state on the work plan. Derive the "last missing" predicate
inline inside `restore_raid1_after_commit()`.

Out of scope: `replace.rs` has its own `will_clear_last_missing` on a
different struct (`ReplaceInput`) with different semantics; do not
touch. `maybe_restore_raid1`'s defensive `pre_op_missing_count == 0`
zero-check (`cli/src/pool.rs:443-476`) stays as-is -- it is public
contract behavior pinned by `maybe_restore_raid1_noop_when_not_degraded`
(`cli/src/pool.rs:1187-1201`).

## Files modified

- `cli/src/remove_missing.rs` (only)

## Changes

### 1. Remove the field from the struct

`cli/src/remove_missing.rs:97-106` -- delete `will_clear_last_missing:
bool,`:

```rust
#[derive(Debug, Clone)]
struct RemoveMissingWorkPlan {
    missing_id: u64,
    target_uuid: LuksUuid,
    target_name: DiskName,
    remaining_present: usize,
    missing_count: u64,
    mount_point: MountPoint,
}
```

### 2. Derive the predicate from `missing_count`

`cli/src/remove_missing.rs:109-111` -- read from `missing_count`:

```rust
fn restore_raid1_after_commit(&self) -> bool {
    self.missing_count == 1 && self.remaining_present >= 2
}
```

### 3. Drop the planner-side `let` and field write

`cli/src/remove_missing.rs:492-502` -- remove the
`will_clear_last_missing` binding and the field in struct construction:

```rust
let remaining_present = pool.devices.len();
let work_plan = RemoveMissingWorkPlan {
    missing_id: params.missing_id,
    target_uuid,
    target_name,
    remaining_present,
    missing_count: pool.missing_count,
    mount_point: config.mount_point().clone(),
};
```

### 4. Switch the test fixture's parameter to `missing_count: u64`

`cli/src/remove_missing.rs:600-616` -- the fixture API mirrors the prod
struct 1:1; no more `if will_clear_last_missing { 1 } else { 2 }`
magic-mapping in the body:

```rust
#[cfg(test)]
fn remove_missing_work_plan_for_test(
    missing_id: u64,
    missing_count: u64,
    remaining_present: usize,
    mount_point: &MountPoint,
) -> RemoveMissingWorkPlan {
    RemoveMissingWorkPlan {
        missing_id,
        target_uuid: LuksUuid::parse("00000000-0000-0000-0000-000000000001").unwrap(),
        target_name: DiskName::parse("disk-test").unwrap(),
        remaining_present,
        missing_count,
        mount_point: mount_point.clone(),
    }
}
```

### 5. Flip the 5 test call sites

Mechanical: `true` -> `1`, `false` -> `2` (preserves the pre-existing
mapping in the deleted fixture body so per-test scenarios are unchanged).
Surrounding test names already describe the intent that the bool used to
carry.

| Line | Test                                                          | Old arg | New arg |
| ---- | ------------------------------------------------------------- | ------- | ------- |
| 1125 | `work_plan_steps_show_rebalance_when_clearing_last_missing`   | `true`  | `1`     |
| 1142 | `work_plan_steps_omit_rebalance_with_single_survivor`         | `true`  | `1`     |
| 1159 | `work_plan_steps_omit_rebalance_when_not_last_missing`        | `false` | `2`     |
| 1693 | `dry_run_render_targeted_removal_with_balance`                | `true`  | `1`     |
| 1933 | `plan_preview_renders_warn_above_steps`                       | `true`  | `1`     |

## What this does NOT change

- `RemoveMissingWorkPlan.missing_count` stays -- it is the value the
  confirm formatter (`format_remove_missing_confirm`, `:622-671`) and
  `maybe_restore_raid1` (`:297-304`) actually consume.
- `maybe_restore_raid1`'s `pre_op_missing_count == 0` early-exit
  (`cli/src/pool.rs:450-452`) and its pinning test stay -- they are
  contract for a public function, not dead code in the function's own
  scope.
- The journal schema is untouched (`restore_raid1_after_commit: bool` on
  `OpKind::RemoveMissing` already carries the gate; the inner missing
  count was never journaled).
- `replace.rs` is untouched.

## Verification

1. `just test-rust` -- runs `cargo test` for the CLI crate. All five
   `remove_missing` tests that touch the fixture exercise the
   `restore_raid1_after_commit()` predicate (rebalance step shown when
   missing_count == 1 && remaining_present >= 2; omitted otherwise).
   Their assertions on rendered steps stay valid because the derivation
   `missing_count == 1` produces the same bool as the deleted field.
2. `just test-vm` (only if the Rust tests pass) -- the
   `remove-missing` VM tests exercise the planner + execute path
   end-to-end and would catch any unintended behavior change in
   `restore_raid1_after_commit()`.

No fixture refresh, no module/Nix changes, no parser changes.
