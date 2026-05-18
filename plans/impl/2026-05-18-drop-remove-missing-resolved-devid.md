# Plan: drop misnamed `resolved_devid` local in `remove_missing::execute`

## Context

`cli/src/remove_missing.rs:176` declares `let resolved_devid = work_plan.missing_id;` and uses the local at seven sites later in `RemoveMissingPlan::execute`. The name is misleading: `resolve_removal_target` (line 58-66) takes `devid: u64` as an *input* and returns `(LuksUuid, DiskName)` -- the devid is the resolution key, not a result. The local is a literal `Copy` of a `u64` field; nothing was resolved, transformed, or renamed.

The misnomer is drift, not intent. Commit `dc6dc03` introduced the local back when `resolve_removal_target` unwrapped an `Option<u64>` and returned `(u64, String)`; later refactors changed the signature to take `devid: u64` directly, but the local kept its old prefix. The same `execute` function already references `work_plan.missing_id` directly at lines 219 and 225, and uses the same direct-field pattern for sibling fields (`work_plan.remaining_present`, `work_plan.missing_count`, `work_plan.mount_point`). The `resolved_devid` local is the only outlier.

Goal: remove the false "this came from resolve" signal at every call site and make it obvious that the journal / UI / alert hygiene arguments come straight from the plan.

## Scope

Single file, single function: `RemoveMissingPlan::execute` in `cli/src/remove_missing.rs`. No behavior change. No public API change. No test changes.

## Change

1. **Delete** the local at `cli/src/remove_missing.rs:176`:
   ```rust
   let resolved_devid = work_plan.missing_id;
   ```

2. **Inline** `work_plan.missing_id` at each of the seven use sites in `execute`:
   - line 184 -- argument to `format_remove_missing_confirm(...)`
   - line 239 -- `devid:` field in `journal::OpKind::RemoveMissing` for `build_journal`
   - line 253 -- `"pool: removing missing devid {…}..."` `format!` arg
   - line 258 -- `&resolved_devid.to_string()` becomes `&work_plan.missing_id.to_string()`
   - line 269 -- `"pool: missing devid {…} removed"` `format!` arg
   - line 285 -- `devid:` field in `journal::OpKind::RemoveMissing` for `rewrite_journal`
   - line 313 -- `&[resolved_devid]` slice passed to `alert::drop_ghost_acked_for_devids`

   All sites currently consume a `u64` by value, so inlining `work_plan.missing_id` (a `Copy` field on an owned `work_plan` binding) requires no `.clone()`, no `&`, no borrow-checker workaround.

Leave the sibling locals alone:

- `target_uuid` (line 174) -- used 3x, wraps a `LuksUuid` (not `Copy`); the `.clone()` is load-bearing for the `&target_uuid` argument on line 232 and the `fresh_uuid != target_uuid` comparison on line 220.
- `name_to_remove` (line 175) -- one use, but the local is a semantic rename (`target_name` -> "name we're removing"), not a no-op alias.

Neither shares the `resolved_devid` failure mode and neither is in scope.

## Files to modify

- `cli/src/remove_missing.rs` -- only.

## Verification

- `just test-rust` -- the `remove_missing` module has unit tests (e.g. `plan_remove_missing_rejects_wrong_missing_id_from_pool_state`, the `resolve_removal_target` tests added in `dc6dc03`, the heartbeat / journal-recovery tests around lines 2061-2652). Behavior is unchanged, so they should all stay green.
- `cargo build -p braid-cli` (covered by `just test-rust`) -- catches any missed substitution.
- No VM tests, no fixture refresh, no docs update needed: this is a private-identifier cleanup inside one function.
