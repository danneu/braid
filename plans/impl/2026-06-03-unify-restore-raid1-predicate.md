# Plan: unify the plan-time `restore_raid1_after_commit` predicate

## Context

`braid`'s two pool-mutation commands that can clear the last missing device --
`remove-missing` and `replace` (missing path) -- both decide at plan time whether
the post-commit soft RAID1 rebalance should be attempted, journal that decision as
`restore_raid1_after_commit`, and later call `crate::pool::maybe_restore_raid1`,
which re-checks the *actual* post-state and owns the final go/no-go.

The plan-time predicate `<clears last missing> && <present survivors> >= 2` is
currently inlined in **three** distinct places with no shared definition:

- `cli/src/remove_missing.rs#RemoveMissingWorkPlan::restore_raid1_after_commit`
  (a method: `missing_count == 1 && remaining_present >= 2`)
- `cli/src/remove_missing.rs#format_remove_missing_confirm` (line ~676: an inline
  copy gating the operator-facing "Data on remaining disks will be rebalanced..."
  message)
- `cli/src/replace.rs#build_replace_work_plan` (line ~1605:
  `will_clear_last_missing && remaining_present >= 2`, stored as a **field** on
  `ReplaceWorkPlan`)

This produces two problems. (1) **Inconsistency between siblings:** `replace.rs`
stores the result as a precomputed field and reads it from `render_steps`/`execute`;
`remove_missing.rs` recomputes via a method and rebinds it in `execute`. (2) **A
real lockstep hazard in the confirm copy:** the `:676` predicate gates what the
operator is *told* will happen, independently of the flag that is *journaled* and
that gates the actual balance step. If one is edited and the other is not, the
prompt and the action silently disagree.

Note the original finding framed `maybe_restore_raid1`'s post-probe gate
(`pool_after.missing_count == 0 && pool_after.devices.len() >= 2`) as a fourth
duplicate that risks desync. It is **not** -- it intentionally measures the real
post-state and is the authoritative re-check. The plan-time predicate is advisory
(it only gates whether the balance phase is *attempted*). This plan keeps that
separation; it does not touch `maybe_restore_raid1`'s logic.

Intended outcome: one named, documented definition of the plan-time predicate,
living at the layer that owns the RAID1-restore invariant (`pool.rs`, next to
`maybe_restore_raid1`), consumed by every site. Pure refactor -- behavior is
identical everywhere.

## Approach (full unification)

### 1. Add one shared helper in `cli/src/pool.rs`

Place it next to `maybe_restore_raid1` so the plan-time gate and the authoritative
post-probe gate are visibly paired:

```rust
/// Plan-time predicate for whether a pool-mutation op (remove-missing, or
/// replace's missing path) will leave the pool non-degraded with enough
/// survivors to re-mirror. Single source for the `restore_raid1_after_commit`
/// journal flag, the dry-run preview step, and the operator confirmation, so
/// all three always agree. Advisory only: `maybe_restore_raid1` re-checks the
/// real post-state and owns the final go/no-go on the soft balance.
pub(crate) fn should_restore_raid1(clears_last_missing: bool, present_after: usize) -> bool {
    clears_last_missing && present_after >= 2
}
```

The topology-specific arithmetic stays at each call site (this is correct -- it
differs per command): `remove-missing` passes `present_after = pool.devices.len()`
(removing a *missing* entry does not change the present count); `replace` passes
`present_after = pool.devices.len() + 1` (the new device fills the cleared slot).
Add a one-line `//` note on `maybe_restore_raid1` pointing back to
`should_restore_raid1` as its plan-time counterpart.

### 2. `cli/src/remove_missing.rs`: method -> field, adopt the helper

- Remove the `RemoveMissingWorkPlan::restore_raid1_after_commit` **method**; add a
  `restore_raid1_after_commit: bool` **field** to the struct (mirrors
  `ReplaceWorkPlan`). A short `//` field note: "advisory plan-time gate; see
  `crate::pool::should_restore_raid1`".
- `render_steps`: change `if self.restore_raid1_after_commit()` to read the field
  `if self.restore_raid1_after_commit`.
- `plan_remove_missing` (the real constructor, ~line 466): set the field with
  `crate::pool::should_restore_raid1(pool.missing_count == 1, remaining_present)`.
- `remove_missing_work_plan_for_test` (~line 646): set the field the same way from
  its `missing_count` / `remaining_present` params, using the same fully-qualified
  `crate::pool::should_restore_raid1(...)` path. (This helper is `#[cfg(test)]` at
  module scope -- *not* inside `mod tests` -- so it resolves paths exactly like the
  non-test code. This mirrors `replace.rs`'s test helper, which recomputes the
  predicate via its real builder rather than hardcoding a bool.)
- `execute` (~line 215): delete the rebind
  `let restore_raid1_after_commit = work_plan.restore_raid1_after_commit();` and
  pass `work_plan.restore_raid1_after_commit` directly into both `build_journal`
  and `rewrite_journal` (it is `Copy`).
- `format_remove_missing_confirm` (~line 676): replace the inline
  `if missing_count == 1 && remaining_present >= 2` with
  `if crate::pool::should_restore_raid1(missing_count == 1, remaining_present)`. Signature
  and the other two message branches are unchanged, so the existing
  `format_remove_missing_confirm("disk3", 3, 2, 1)` test still passes.

### 3. `cli/src/replace.rs`: route the third copy through the helper

- `build_replace_work_plan` (~line 1605): replace
  `let restore_raid1_after_commit = will_clear_last_missing && remaining_present >= 2;`
  with
  `let restore_raid1_after_commit = crate::pool::should_restore_raid1(will_clear_last_missing, remaining_present);`
  Everything else in `replace.rs` (the field, `render_steps` reading it, the test
  helper funneling through this builder) already follows the target idiom and is
  unchanged.

## Reuse / references

- Target idiom already in the tree:
  `cli/src/replace.rs#ReplaceWorkPlan` (field) +
  `cli/src/replace.rs#build_replace_work_plan` (compute-once) +
  `render_steps` reading the field.
- Authoritative counterpart that stays as-is:
  `cli/src/pool.rs#maybe_restore_raid1`.
- Call the helper fully-qualified as `crate::pool::should_restore_raid1(...)` at
  every site, matching the existing `crate::pool::maybe_restore_raid1` calls already
  in both files (`remove_missing.rs:279`, `replace.rs:916`). Neither file imports
  `pool` as a module or via a glob -- they import *specific* items
  (`use crate::pool::pool_remove_device_using;` at `remove_missing.rs:10`,
  `use crate::pool::{pool_replace_device, pool_resize_device};` at `replace.rs:20`).
  Under edition 2024 (`cli/Cargo.toml:4`) a bare `pool::` path from a sibling module
  does not resolve against the crate root (E0433), so the `crate::` prefix is
  required and **no new `use` is needed or wanted** (don't add `use crate::pool;`).

## Explicitly unchanged (blast-radius bound)

- `cli/src/journal.rs` -- `OpKind::RemoveMissing { restore_raid1_after_commit, .. }`,
  `build_journal`, `rewrite_journal` signatures and the journaled value are
  untouched.
- `cli/src/recover.rs` -- reads the journaled bool; no change.
- `cli/src/pool.rs#maybe_restore_raid1` -- post-probe logic unchanged (doc note only).
- No behavioral change anywhere: the predicate value is identical at every site.

## Verification

This is a logic-preserving refactor confined to the Rust CLI; no parser, tool
version, systemd, or mount behavior changes, so no fixture refresh and no VM tests
are required.

1. `just test-rust` -- full Rust unit suite. Covers the `remove_missing`,
   `replace`, `journal`, and `pool` (`maybe_restore_raid1_*`) tests. All should
   pass unchanged because behavior is identical.
2. `cargo build` (or `cargo clippy`) -- confirm the new `pub(crate)` helper is
   warning-clean.
3. Spot-confirm the predicate parity by reading the diff: each of the three former
   inline expressions now calls `should_restore_raid1` with the same operands it
   previously combined inline.

Optional belt-and-suspenders (only if the user wants broad coverage): the
recover/replay VM tests exercise the journaled `restore_raid1_after_commit` flag,
but since the journal value is unchanged they are not expected to be affected.

## Implementation notes

- The plan called for a one-line `//` note on `maybe_restore_raid1` pointing at
  `should_restore_raid1`. Implemented as two trailing lines inside the existing
  `///` doc comment instead of a separate `//` comment, so the pairing stays in
  rustdoc rather than being a code-only aside.
