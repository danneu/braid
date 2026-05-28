# Plan: remove-missing rebalance promise / silent-skip divergence

## Context

A `/code-review` finding (project-fit, Low severity) flagged that
`format_remove_missing_confirm` writes "Data on remaining disks will be
rebalanced." at confirm time based on plan-time pool state, but
`maybe_restore_raid1` re-probes the pool after the device-remove and
**silently** skips the soft balance if the pool is still degraded
(`pool_after.missing_count > 0` or `devices.len() < 2`).

Two narrow but real divergences result:

1. **Prompt overpromise.** If another device drops between confirm and
   the post-op probe (e.g. mid-operation drive failure), the user
   confirmed on a promise that didn't fire.
2. **Silent skip.** When the balance is skipped, no status output tells
   the user why -- the operation appears to succeed end-to-end with no
   indication the promised rebalance didn't run.

Neither is a safety issue: the re-probe gate is the correct fail-safe
and the pool is not damaged. The fix is UX consistency -- make the
prompt accurate AND make execution transparent.

The proposed fix in the original finding was only the wording softening.
The ideal fix closes both gaps so the confirm prompt remains accurate
even in the rare skip case AND the user always learns the outcome.

## Approach

### Part 1: Soften the confirm prompt (`cli/src/remove_missing.rs:623`)

Change the unconditional promise:

```rust
msg.push_str("  Data on remaining disks will be rebalanced.\n");
```

to a conditional statement that matches `maybe_restore_raid1`'s
execute-time gate:

```rust
msg.push_str("  Data on remaining disks will be rebalanced if redundancy is restored.\n");
```

Rationale: the conditional phrasing acknowledges the post-op probe gate
without adding noise for the common case (where the balance does run).
It also distinguishes itself from the multi-missing branch's "Pool will
remain degraded" message and from the single-survivor branch's
"Surviving disk already has all data" -- both of which already
correctly omit any rebalance promise.

The existing positive assertion at `remove_missing.rs:2071`
(`assert!(msg.contains("rebalanced"));`) still passes with the new
wording. The existing negative assertion at `:2125`
(`!msg.contains("rebalanced")`) is for the multi-missing branch, which
is unchanged.

Tighten `remove_missing_confirm_with_rebalance` at `:2065-2073` so its
positive assertion pins the new conditional phrasing, not just the bare
word "rebalanced" -- otherwise a future accidental revert to the
unconditional promise would still pass the test. Concretely:

```rust
assert!(msg.contains("rebalanced if redundancy is restored"));
```

### Part 2: Emit `[skip]` status when the balance is skipped (`cli/src/pool.rs:462-495`)

Currently the skip branch returns `Ok(())` with no user-facing output.
Add a `StatusTag::Skip` line that names the reason. The codebase
already uses `StatusTag::Skip` for no-op outcomes (see
`cli/src/credential_verify.rs:111` "keyfile: not yet enrolled on X").
Standalone `[skip]` rows are allowed by the status-line contract in
`docs/design/principles.md:97-100` -- the rule only constrains
`[wait]` rows, which must be closed by one of `[ok]`/`[fail]`/`[warn]`/
`[skip]` or a non-zero exit. The existing success path's Wait/Ok pair
is unchanged.

The skip body must be **caller-neutral**: `maybe_restore_raid1` is a
shared helper called by both `remove-missing`
(`cli/src/remove_missing.rs:278`) and missing-path `replace`
(`cli/src/replace.rs:931`). Phrasing like "after removal" would be
wrong for the replace caller. Use:

> `pool: rebalance skipped -- redundancy not restored`

Route all three status lines (Wait, Ok, new Skip) through `emit_status`
(`cli/src/status_tag.rs:66`) instead of raw `eprint!`. This is the
project convention (see `cli/src/add.rs:409` and the regression-guard
comment at `cli/src/enroll_key_file.rs:2241-2247` explicitly warning
that reverting to raw `eprint!` "silently regresses" the
`emit_status` test seam). The existing `eprint!` calls inside
`maybe_restore_raid1` are pre-existing inconsistency; folding them
into `emit_status` at the same time keeps all three rows uniform and
makes the function fully testable via the existing
`status_tag::testing::capture_with_color` seam.

Updated structure (in `maybe_restore_raid1`):

```rust
let color_enabled = color_enabled_for_stderr();
if pool_after.missing_count == 0 && pool_after.devices.len() >= 2 {
    emit_status(&status_line(StatusTag::Wait, color_enabled,
        "pool: restoring RAID1 redundancy..."));
    pool_balance_raid1_soft(runner, mount_point, progress)?;
    emit_status(&status_line(StatusTag::Ok, color_enabled,
        "pool: RAID1 redundancy restored"));
} else {
    emit_status(&status_line(StatusTag::Skip, color_enabled,
        "pool: rebalance skipped -- redundancy not restored"));
}
```

The single skip body covers both probe-gate failure modes
(`missing_count > 0` and `devices.len() < 2`) because the user-facing
outcome is the same: pool didn't return to full redundancy, so the
balance didn't run. A more granular split would be noise.

Note: `maybe_restore_raid1` is called only when
`pre_op_missing_count > 0` (it short-circuits to `Ok(())` otherwise --
no skip emitted in that case because the user was never promised a
balance to begin with).

## Files to modify

- **`cli/src/remove_missing.rs`** -- update line 623 (wording) and
  tighten the positive assertion at `:2071` to pin the new conditional
  phrasing.
- **`cli/src/pool.rs`** -- in `maybe_restore_raid1` (lines 474-493),
  hoist `color_enabled_for_stderr()`, replace the three `eprint!`
  calls with `emit_status`, and add the `else` branch with the
  caller-neutral `[skip]` status line. Add `emit_status` to the
  imports. No signature change, no caller change. Update
  `maybe_restore_raid1_skips_when_still_degraded` at `:1172` to
  capture and assert the `[skip]` row.

## Reused helpers

- `status_line(StatusTag::Skip, color_enabled, body)` -- defined in
  `cli/src/status_tag.rs:58`. Existing precedent for `StatusTag::Skip`
  is `cli/src/credential_verify.rs:111`.
- `emit_status(line)` -- defined in `cli/src/status_tag.rs:66`.
  Routes to `eprint!` in production and to the test capture buffer
  under `#[cfg(test)]`. Replace all three current `eprint!` calls in
  `maybe_restore_raid1` with `emit_status`. Widely used elsewhere
  (e.g. `cli/src/add.rs:409`, `cli/src/enroll_key_file.rs:212`).
- `status_tag::testing::capture_with_color` -- defined in
  `cli/src/status_tag.rs:139`. Captures `emit_status` output for unit
  tests. Existing precedent: `cli/src/enroll_key_file.rs:1626`
  (`plan_enroll_dry_run_emits_keyfile_probe_rows_via_emit_status`).
- `color_enabled_for_stderr()` -- already used in the success branch
  of `maybe_restore_raid1`; hoist its call out of the success branch
  so both branches can reuse it.

## Tests

- **Update** `remove_missing_confirm_with_rebalance` at
  `cli/src/remove_missing.rs:2065`: tighten the positive assertion to
  `msg.contains("rebalanced if redundancy is restored")` so a
  regression to the unconditional promise fails the test.
- **Update one of the existing `maybe_restore_raid1_skips_*` tests**
  (pick `maybe_restore_raid1_skips_when_still_degraded` at
  `cli/src/pool.rs:1172` -- the realistic race) to also pin the new
  `[skip]` row. Wrap the call in
  `crate::status_tag::testing::capture_with_color(false, || { ... })`
  and assert the captured buffer contains exactly:

  ```text
  [skip] pool: rebalance skipped -- redundancy not restored
  ```

  This anchors the user-facing line so a future change that deletes
  the `else` branch, drops the emit, or reverts to raw `eprint!`
  fails `just test-rust`. Follow the existing pattern at
  `cli/src/enroll_key_file.rs:1626`
  (`plan_enroll_dry_run_emits_keyfile_probe_rows_via_emit_status`).
  Leave the sibling `maybe_restore_raid1_skips_single_device` test as
  no-balance-only to keep coverage of the second skip branch's
  no-balance behavior intact.
- **Existing tests stay green without change**:
  - `remove_missing_confirm_single_survivor` at
    `cli/src/remove_missing.rs:2076` (single-survivor branch is
    unchanged).
  - `remove_missing_confirm_multiple_missing` at
    `cli/src/remove_missing.rs:2098` (multi-missing branch is
    unchanged; `!msg.contains("rebalanced")` still holds).
  - `maybe_restore_raid1_skips_single_device` at
    `cli/src/pool.rs:1189` (still asserts no balance + Ok return).
  - The existing success-path tests (e.g.
    `three_device_pool_soft_rebalance_runs` at
    `cli/src/remove_missing.rs:1553`) stay green: routing the Wait/Ok
    lines through `emit_status` does not change production behavior
    (it routes to `eprint!` outside `#[cfg(test)]`).

## Verification

1. `just test-rust` -- runs the updated formatter test, the updated
   `maybe_restore_raid1_skips_when_still_degraded` test (now
   asserting the `[skip]` row via `capture_with_color`), and all
   other formatter/skip tests. All must pass.
2. Manually inspect the wording: read `cli/src/remove_missing.rs:611-660`
   and confirm the three branches read coherently end-to-end (the
   "rebalanced if redundancy is restored" line should sit naturally
   next to "Surviving disk already has all data" and "Pool will remain
   degraded -- X missing Y will remain").
3. **No VM test run required.** The change is wording + a single
   stderr line in an already-tested code path; the VM tests that
   exercise `remove-missing` (e.g. `three_device_pool_soft_rebalance_runs`
   at `:1553`) cover the success path and remain unchanged. If the
   user wants belt-and-suspenders, the focused VM check is
   `just test-vm remove-missing` (or the relevant test name once
   confirmed in `tests/`); this is not strictly required by the change.

## Out of scope

- No change to `format_replace_confirm` (`cli/src/replace.rs:1922`) --
  it does not make an analogous rebalance promise. The only data
  promise there is "Data will be rebuilt from RAID redundancy" for the
  `is_rebuild` path, which is unrelated to the soft-balance follow-up.
- No change to the `maybe_restore_raid1` function signature, journal
  schema, or planner.
- No new design decision doc -- the silent-skip behavior was not
  explicitly designed (no ADR), so removing the silence does not
  contradict any documented invariant. `docs/design/principles.md:21`
  frames the soft balance as a mandatory follow-up when the conditions
  hold, which the new Skip emit makes more legible, not less.
