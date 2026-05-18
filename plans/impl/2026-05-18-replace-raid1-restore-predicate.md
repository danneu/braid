# Plan: clarify `restore_raid1_after_commit` predicate in `replace.rs`

## Context

`cli/src/replace.rs:1505-1507` computes the `restore_raid1_after_commit`
flag as:

```rust
let restore_raid1_after_commit = matches!(&input.replace_source, ReplaceSource::Missing { .. })
    && input.pool.missing_count == 1
    && input.pool.devices.len() + 1 >= 2;
```

The final conjunct is obscure. The `+1` is the new replacement device
and the `>= 2` is the RAID1 minimum, so the conjunct is the post-replace
live count, but a cold reader has to do the algebra themselves.

The sibling code path -- `cli/src/remove_missing.rs:108-111`, set up at
`492-493` -- already expresses the same predicate clearly using named
locals (`will_clear_last_missing` and `remaining_present`). Both call
sites feed the same downstream helper, `crate::pool::maybe_restore_raid1`
(`cli/src/pool.rs:443-476`), which re-probes and re-guards on the
post-op state, so the planner-side predicate is purely a render/journal
gate.

The review finding proposed a different fix -- substitute
`pool.total_devices >= 2` -- which is mathematically equivalent under
the `missing_count == 1` precondition but trades one algebraic shortcut
(`+1 >= 2`) for another (relying on `total_devices == devices.len() +
missing_count`). The cleaner outcome is to mirror the sibling pattern,
so a future reader scanning either command finds the same idiom.

## Approach

Refactor only the computation site at
`cli/src/replace.rs:1505-1507`. Introduce two named locals matching the
naming convention from `remove_missing.rs`, then compose the final bool
from them:

```rust
let will_clear_last_missing =
    matches!(&input.replace_source, ReplaceSource::Missing { .. })
        && input.pool.missing_count == 1;
// +1: the new device added by this replace fills the cleared missing slot.
let remaining_present = input.pool.devices.len() + 1;
let restore_raid1_after_commit = will_clear_last_missing && remaining_present >= 2;
```

The `ReplaceWorkPlan.restore_raid1_after_commit: bool` field
(`cli/src/replace.rs:239`) and the journal schema field of the same name
(`cli/src/journal.rs:194`) are unchanged. This is a planning-site-only
readability refactor.

### Why not the broader Option B

Mirroring `remove_missing.rs` structurally -- adding
`will_clear_last_missing` and `remaining_present` as fields on
`ReplaceWorkPlan`, removing the precomputed bool, exposing
`restore_raid1_after_commit()` as a method -- would ripple through
`execute()`'s destructure (`cli/src/replace.rs:415-432`), `render_steps`
(`373`), and the journal write/rewrite sites (`599`, `885`). The
predicate is already gated by `pool::maybe_restore_raid1`'s own re-probe,
and the post-op count isn't referenced elsewhere in `replace.rs`, so the
extra fields would carry no additional use beyond mirroring. Out of
scope.

### Why not the finding's `pool.total_devices >= 2`

Correct under the `missing_count == 1` precondition, but:

- Does not match the sibling pattern in `remove_missing.rs`; the two
  predicates would still look different.
- Reads as a pre-op statement ("the pool already has 2 devices") rather
  than the post-op semantic the code actually expresses.

## Files to modify

- `cli/src/replace.rs` -- lines 1505-1507 only.

## Reused existing utilities / patterns

- Naming convention from `cli/src/remove_missing.rs:102-110` and
  `492-493`: `will_clear_last_missing: bool`, `remaining_present: usize`.
- Downstream gate (re-probe + re-guard) in
  `crate::pool::maybe_restore_raid1`
  (`cli/src/pool.rs:443-476`), unchanged.

## Verification

The predicate is already pinned at both polarities by existing tests in
`cli/src/replace.rs`, all of which route through `build_replace_work_plan`
via the `replace_work_plan_for_test` helper (`cli/src/replace.rs:1751`)
or full `plan_replace` / `cmd_replace` flows. The refactor preserves the
boolean exactly, so all must continue to pass without modification:

False-case (predicate must return `false`, rendered steps must NOT
contain `-dconvert=raid1,soft`):

- `dry_run_missing_not_last_omits_rebalance`
  (`cli/src/replace.rs:2934`) -- missing source, `missing_count >= 2`.
- `dry_run_missing_single_device_omits_rebalance`
  (`cli/src/replace.rs:2965`) -- missing source, `missing_count == 1`
  but `total_devices == 1` (so `devices.len() + 1 == 1`).
- `dry_run_live_path_no_soft_balance`
  (`cli/src/replace.rs:3454`) -- live source (the `matches!` guard
  short-circuits regardless of counts).

True-case (predicate must return `true`, rendered steps MUST contain
`-dconvert=raid1,soft`):

- `plan_replace_missing_preview_has_no_notes_and_matches_legacy_step_render`
  (`cli/src/replace.rs:4515`) -- 1 live + 1 missing fixture; asserts
  `rendered.contains("-dconvert=raid1,soft")`.
- `pool_json_persisted_when_missing_path_soft_balance_fails`
  (`cli/src/replace.rs:4207`) -- end-to-end `cmd_replace` on the same
  fixture; the soft-balance step is stubbed to fail, which can only
  happen if the predicate was `true` and the step was scheduled.

Run:

```sh
just test-rust
```

If the boolean changes for any input, those tests fail. No new tests are
needed -- this is a pure structure-preserving rename of intermediate
expressions, and the behavioral coverage already exists at both
polarities.
