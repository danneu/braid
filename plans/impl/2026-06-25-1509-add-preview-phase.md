# Pivot: single-source the add *preview's* balance/bootstrap prediction

> **Re-grounded on the post-`1e39b948` tree.** Two commits landed after this plan's
> first drafts and reshaped the target code, both in the same "plan-time prediction
> vs. live state" area this plan lives in:
>
> - `1af97ff3` ("fix(add): gate raid1 balance on fresh pool probe") made
>   `AddPlan::execute` stop recomputing the plan-time balance gate; it now gates the
>   hard convert on a fresh post-add `pool_after` probe (`pool_can_host_raid1`).
> - `1e39b948` ("fix(add): make live balance gate own skip output", current `HEAD`;
>   impl plan `plans/impl/2026-06-25-1429-add-live-balance-skip-gate.md`) made
>   execute *filter* the plan-time `[skip]` note out of its real-run replay and
>   *widened* its degraded branch to `pool_after.missing_count > 0`, so execute's
>   live gate is now the sole real-run emitter of the skip line for every degraded
>   post-add outcome (member dropped *or* returned after planning).
>
> The earlier framing ("triplication", "execute violates ADR 022") is false against
> the tree and has been removed. **The plan's core mechanism is unaffected:** both
> commits touched execute's note-replay and branch condition, *not* `render_steps`,
> the `plan_add` note gate, or the three scalars -- so collapsing the scalars into a
> typed `preview_phase` read by the two plan-time preview readers, with execute left
> untouched, still applies verbatim. This is a **re-grounding/rescope, not an
> abort**: the win is two plan-time preview readers, not three sites. See "Honest
> value reassessment" below.

## Context

`braid add` spells its post-add RAID1 convert-balance prediction **twice**, from
the same plan-time source, across two preview-side readers:

- `AddWorkPlan::render_steps` (dry-run steps) reads three threaded scalars on
  `AddWorkPlan` -- `pool_was_mounted`, `existing_pool_device_count`,
  `pre_add_missing_count` -- to decide bootstrap-vs-live and whether to render the
  balance step.
- The degraded-balance `[skip]` note in `plan_add` reads the **same three
  scalars** with the **same math** (`pool_was_mounted && total_after >= 2 &&
  pre_add_missing_count > 0`) to predict that the convert will be skipped. Since
  `1e39b948` this note is *dry-run-only*: execute filters it from the real-run
  replay and re-emits the skip line from its own live gate. That sharpens it as a
  pure *preview* reader -- exactly the plan-time advisory this refactor single-sources.

The three scalars are verbatim copies of plan-time `PoolState` fields, assigned
together in `build_add_work_plan`. The two readers hand-maintain one predicate:
change the gate in one and forget the other and dry-run stdout (the step list)
and the `[skip]` note disagree about the same run.

**This is a duplication, not a triplication, and `AddPlan::execute` is *not* a
third copy.** Since `1af97ff3` + `1e39b948`, execute makes an *independent,
authoritative* balance decision from the fresh post-add probe `pool_after` via
`pool_can_host_raid1(&pool_after)` (`cli/src/pool.rs#pool_can_host_raid1` =
`missing_count == 0 && devices.len() >= 2`). For the real-run skip line it does
**not** replay the plan-time `[skip]` note -- it filters `PreviewNote::Skip` out of
the replay and re-emits a fresh skip from its live gate whenever the post-add probe
is still degraded (`else if pool_after.missing_count > 0`), for *every* degraded
outcome, not just newly-degraded ones. That gate deliberately does **not** read the
plan-time scalars -- it closes the confirmation/passphrase/format/add window where a
member can drop *or return* after planning.

**ADR 022 framing (corrected).** Execute's fresh-probe gate is **not** an ADR 022
violation -- it is the "execution-time validation that dry-run intentionally
cannot do" that ADR 022 (`docs/design/decisions/022-dry-run-preview-model.md`)
*permits*. `docs/internals/btrfs/balance-soft.md` ("Skip -- degraded add") already
documents this as a deliberate **advisory-plan / authoritative-execute** split,
the same shape as `should_restore_raid1` (plan-time advisory) plus
`maybe_restore_raid1` (execute re-probe). The motivation for this plan is
therefore **not** "kill an ADR 022 violation"; it is narrower: the two *plan-time
advisory readers* hand-maintain one scalar predicate -- single-source it.

**Outcome:** compute the preview prediction *once* at plan time, store it as a
typed field on `AddWorkPlan`, and have the two preview readers (and only those)
read that one value. `AddPlan::execute` is left **entirely untouched**.

### Why this shape (precedent, not invention)

- **Storage pattern.** `ReplaceWorkPlan` precomputes typed decisions
  (`restore_raid1_after_commit: bool`, `target_prep: ReplaceTargetPrep`) and
  stores them on the work plan because the renderer cannot re-derive them; the
  same "render can't see `PoolState`, so store the decision" rationale applies
  here. `LockPlan` (`close_set: LockCloseSet`) and `RecoverWorkPlan` do likewise.
- **Honest caveat on the precedent.** In `ReplaceWorkPlan` *both* render and
  execute read the stored decision. Here, by design, **execute does not** -- it
  keeps its own authoritative `pool_after` gate. So this field is purely the
  *preview predictor*; the precedent is the storage mechanics, not "preview and
  execute share one value." Keeping execute independent matches the
  `should_restore_raid1` / `maybe_restore_raid1` asymmetry that `1af97ff3` +
  `1e39b948` aligned `add` with.

## Soundness (the only equivalence that must hold)

The whole prior `targets.len() == needs_pool_add.len()` proof is **moot** for this
plan -- it mattered only because execute was treated as a consumer of the gate,
and it no longer is. The single equivalence to preserve is **render_steps <->
`[skip]` note**, and it is trivial:

- Both readers run at plan time and currently derive their decision from the same
  three scalars, themselves copies of one plan-time `PoolState` snapshot taken in
  `build_add_work_plan`.
- Routing both through `add_preview_phase(&input.pool, targets.len())`, computed
  once from that same snapshot, is identical by construction. There is no second
  source to reconcile and no execute involvement.

## Design

### New types (in `cli/src/add.rs`)

Names signal **preview-only** scope so no future reader wires execute to them.
Both enums are `Copy` (fieldless / wrap-fieldless): the two readers access the
field through a shared reference (`&self` in `render_steps`, `&work_plan` in the
note), so `Copy` lets them read it by value without borrow gymnastics.
`PartialEq, Eq` back the note's `==` comparison and the decision-table unit test's
`assert_eq!`; `Debug, Clone` satisfy `AddWorkPlan`'s own `#[derive(Debug, Clone)]`.

```rust
/// Plan-time *preview prediction* of the add pool phase, decided once from the
/// planning `PoolState` + work targets so the dry-run step builder and the
/// `[skip]` note share one source instead of two. PREVIEW ONLY:
/// `AddPlan::execute` makes the authoritative balance call independently from the
/// fresh post-add `pool_after` probe (`pool_can_host_raid1`) and may diverge from
/// this prediction when a member drops *or returns* after planning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AddPreviewPhase {
    /// Pool not mounted -> mkfs + mount. Single vs RAID1 topology follows the
    /// target count (intrinsic to the work plan), so it stays a local check.
    Bootstrap,
    /// Pool live -> `btrfs device add` each fresh target, then a predicted balance.
    LiveAdd(PreviewedBalance),
}

/// Plan-time *prediction* of the post-`device add` RAID1 hard convert
/// (balance-soft.md). Consumed only by the preview step builder and the `[skip]`
/// note. Execute decides the real go/no-go from `pool_after`, so a `Run`
/// prediction does NOT guarantee execute runs the convert.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PreviewedBalance {
    /// total_after >= 2, plan-time pool whole: preview predicts the hard
    /// `-dconvert=raid1` will run (execute confirms against `pool_after`).
    Run,
    /// total_after >= 2, plan-time pool still degraded (missing member): preview
    /// predicts the convert is skipped; emit ONE `[skip]` note.
    SkipDegraded,
    /// total_after < 2 (defensive lower bound): no balance, no note.
    NotApplicable,
}

/// Single source of the add preview-phase prediction. Called by the planner and
/// by the one hand-built test work plan so both compute it identically.
fn add_preview_phase(pool: &PoolState, target_count: usize) -> AddPreviewPhase {
    if !pool.mounted {
        return AddPreviewPhase::Bootstrap;
    }
    let total_after = pool.devices.len() + target_count;
    let balance = if total_after < 2 {
        PreviewedBalance::NotApplicable
    } else if pool.missing_count == 0 {
        PreviewedBalance::Run
    } else {
        PreviewedBalance::SkipDegraded
    };
    AddPreviewPhase::LiveAdd(balance)
}
```

### `AddWorkPlan` struct

Drop `pool_was_mounted`, `existing_pool_device_count`, `pre_add_missing_count`.
Add one field: `preview_phase: AddPreviewPhase`. Delete the 3-field/predictor doc
comment that pinned the copies (its "execute re-checks the authoritative post-add
probe" note moves into the enum doc above).

### Population (`build_add_work_plan`)

Replace the three scalar assignments with
`preview_phase: add_preview_phase(&input.pool, targets.len())` (adjust the `&` to
`input.pool`'s actual type).

### Consumers rewired -- exactly two (both plan-time)

1. **`render_steps`** (`AddWorkPlan::render_steps`): replace
   `if !self.pool_was_mounted { ... } else { ... }` with `match self.preview_phase`.
   The `Bootstrap` arm keeps its local `self.mapper_paths().len() >= 2` topology
   split (intrinsic to the work plan, can't drift). The `LiveAdd(balance)` arm
   renders the balance step iff `balance == PreviewedBalance::Run` -- the
   `total_after` math and `missing == 0` test are gone (folded into the enum).
2. **degraded `[skip]` note** (in `plan_add`): becomes
   `if !work_plan.is_noop() && work_plan.preview_phase ==
   AddPreviewPhase::LiveAdd(PreviewedBalance::SkipDegraded) { ... }`. The
   `!is_noop()` guard is **required**: `preview_phase` is not meaningful for plans
   that do no work, and a degraded *no-op* re-add into an already-degraded
   multi-present pool (e.g. 2 present + 1 missing -> `total_after = 2`) *does* map
   to `LiveAdd(SkipDegraded)`. `is_noop()` is the sole no-op authority; it -- not
   the phase -- keeps a no-op silent. (Preserves today's behavior: the current
   note gate is likewise `!is_noop()`-first.) This gate produces only the
   plan-time/dry-run `[skip]` note; since `1e39b948` execute filters that note from
   its real-run replay and re-emits the skip from its live gate, so rewiring the
   gate never touches real-run output.

`AddPlan::execute` is **not** in this list. It is not rewired at all (see below).

### What deliberately stays unchanged

- **`AddPlan::execute`'s balance gate is untouched.** It keeps
  `pool_can_host_raid1(&pool_after)` for the hard convert and the widened
  `else if pool_after.missing_count > 0` real-run skip branch -- which since
  `1e39b948` owns the skip output for *every* degraded post-add outcome (member
  dropped or returned), paired with execute filtering the plan-time
  `PreviewNote::Skip` out of its replay so the line is emitted exactly once from the
  live gate. It reads `pool_after` (fresh) and `self.pool`, never `preview_phase`.
  This is the whole point of the rescope: wiring execute to the plan-time prediction
  would revert **both** `1af97ff3` (reopening the newly-degraded-add bug) and
  `1e39b948` (reopening the stale-skip-replay bug). Execute's own
  `!self.pool.mounted` bootstrap-vs-live branch is **also** left as-is -- reading the
  `Bootstrap`/`LiveAdd` discriminant while ignoring the authoritative balance call
  would be a confusing half-read.
- **`AddPlan.pool` is kept.** Execute reads `self.pool` for execution-time
  *identity validation* (the `classify_braid_disk_fsid` and
  `validate_execute_pool_identity` guards) and for its `!self.pool.mounted`
  bootstrap-vs-live branch. An external test also reads `plan.pool.missing_count`
  (`plan_add_degraded_noop_keeps_missing_warning`). Note: `1e39b948` removed
  execute's *only* `self.pool.missing_count` read -- the old divergence-skip branch
  was `self.pool.missing_count == 0 && pool_after.missing_count > 0` and is now
  `pool_after.missing_count > 0` -- so that is no longer a reason to keep the field.
  The conclusion (keep `AddPlan.pool`) is unchanged; the rationale is narrower.
- **Bootstrap single-vs-RAID1 topology** stays a local `mapper_paths.len() >= 2`
  check in both render and execute -- a function of target count (intrinsic to the
  work plan), not a pool fact, so it is not part of the drift surface.
- **`is_noop()` early-returns** (in `AddWorkPlan::render_steps` and
  `AddPlan::execute`) remain the no-op authority; `preview_phase` is only
  consulted for real work.

## Files to modify

- `cli/src/add.rs` -- new enums + `add_preview_phase`; `AddWorkPlan` field swap;
  one builder population site; the **two** preview-reader rewires (render_steps +
  the `[skip]` note); the hand-built test literal in `plan_for_execute_target`
  (`pool_was_mounted: pool.mounted, ...` becomes
  `preview_phase: add_preview_phase(&pool, <that literal's target count>)`).
  **No edit to `AddPlan::execute`.**
- `docs/internals/btrfs/balance-soft.md` -- in the "Skip -- degraded add"
  paragraph (rewritten again by `1e39b948`), update *only* the plan-time-advisory
  clause: the surviving edit-target sentence is "`plan_add` and
  `AddWorkPlan::render_steps` key on the plan-time `pre_add_missing_count` as a
  best-effort preview predictor" -- substitute it so those two readers read the
  precomputed `preview_phase` (specifically `LiveAdd(SkipDegraded)`) instead of
  `pre_add_missing_count` directly. **Preserve everything `1e39b948` added:** the
  documented asymmetry (execute gates on the fresh `pool_after` probe, *not* the
  precomputed value) and the new live-state sentences ("if a member drops, the real
  run skips the convert and surfaces the `[skip]` note from the execute gate; if a
  missing member returns, the real run balances and suppresses the previewed skip
  ... keeps the real-run balance-skip line tied to live state instead of replaying a
  stale plan-time prediction"). Do *not* say execute reads `preview_phase`. (The
  earlier draft edit, which had the preview step, the note, *and* execute "all read"
  one precomputed value, contradicts both `1af97ff3` and `1e39b948` and stays dropped.)

No changes to: the `build_add_work_plan(...).render_steps()` test call sites
(`render_steps` signature is unchanged, still `&self`), the `CmdRequest` variants,
or the note formatter (`format_add_degraded_balance_skip`).

## Reuse

- `add_preview_phase` is the single decision helper (planner + test share it).
- Storage pattern mirrors `ReplaceWorkPlan` / `LockPlan` (`cli/src/replace.rs`,
  `cli/src/lock.rs`); the plan-time-advisory-vs-execute-authoritative split mirrors
  `should_restore_raid1` / `maybe_restore_raid1` (`cli/src/pool.rs`).
- `PoolState` (`cli/src/types.rs#PoolState`): `mounted: bool`, `devices: Vec<PoolDevice>`,
  `missing_count: u64`; derives `Clone`.

## Honest value reassessment

The win is smaller than the original pitch and worth stating plainly:

- **Then:** "kill a three-site triplication / ADR 022 violation."
- **Now:** "two plan-time preview readers hand-maintain one scalar predicate;
  collapse three untyped scalars into one typed decision both read identically."

Still defensible: a `match`-able `AddPreviewPhase` makes impossible states
unrepresentable and removes the render<->note drift by construction.
**Alternative considered:** a shared predicate *method* both readers call (no new
field). Rejected as strictly heavier here -- the renderer cannot see `PoolState`,
so a method would still need the three scalars stored *and* add a call; the typed
field is leaner (-3 scalars, +1 field) and removes the same drift. **Flag for
the reviewer:** confirm the enum is worth it at this reduced scope; if not, the
fallback is to keep the scalars but route both readers through one shared
helper -- still removes the drift, less type machinery.

## Behavior preservation

The enum reproduces the two plan-time readers' conditions exactly (see Soundness).
`AddPlan::execute` is not modified, so its behavior -- including the newly-degraded
`pool_after` skip from `1af97ff3` and the filtered-replay / widened-branch
single-emit from `1e39b948` -- is trivially preserved. No rendered step, note
wording, ordering, or execute mutation changes.

## Testing / Verification

The **behavioral regression gate** is the existing suite, which covers both axes
(preview and execute). A regression in the rewire fails loudly there:

- **Hard constraint (proves we did not undo `1af97ff3` or `1e39b948`):** three
  execute tests must stay green and the diff must not touch `AddPlan::execute`'s
  balance gate or note-replay:
  - `execute_newly_degraded_add_skips_raid1_balance_and_emits_note` (plan healthy,
    fresh probe newly degraded -> execute skips the convert and emits the note) --
    the `1af97ff3` guard.
  - `execute_member_returned_add_balances_without_stale_skip_note` (plan degraded,
    missing member returns before the probe -> execute balances and *suppresses* the
    stale plan-time skip note) -- the `1e39b948` guard; it pushes both a Warn and a
    Skip note onto `plan.notes` and asserts the Warn replays while the Skip does not.
  - `execute_degraded_add_skips_raid1_balance` (still-degraded add skips the convert).
  If any changes, the rescope was violated.
- Dry-run preview (the two rewired readers): `plan_add_degraded_preview_omits_balance_step`
  (degraded add omits the balance step and emits one `[skip]` -- it also asserts
  the `btrfs device add` step *still* appears, so a rewrite that dropped the
  device-add loop from the `LiveAdd` arm fails here),
  `plan_add_degraded_noop_keeps_missing_warning` (degraded no-op stays silent on
  the skip note), `dry_run_render_add_to_existing_pool_with_balance` (live add
  renders the balance step), `dry_run_render_fresh_two_disk_bootstrap_uses_raid1_mkfs`
  (bootstrap RAID1 mkfs).
- VM end-to-end: `tests/cli/add-degraded-then-remove-missing.py`,
  `tests/cli/braid-add-warnings.py` (skip note exactly once; no `[wait]`/`[ok]`
  balance lines on degraded add).

**Add one new unit test as an internal helper guard** (not the behavioral
regression gate -- that role stays with the suite above): an `add_preview_phase`
decision table -- unmounted -> `Bootstrap`; mounted whole, `total_after >= 2`
-> `LiveAdd(Run)`; mounted degraded (`missing_count > 0`), `total_after >= 2`
-> `LiveAdd(SkipDegraded)`; mounted, `total_after < 2` (1 present + 0 targets)
-> `LiveAdd(NotApplicable)`. It locks the decision table the two preview readers
now share (relying on the new `PartialEq, Eq` derives for `assert_eq!`).

Commands:

- `just test-rust` -- unit tests (preview readers + the unchanged execute gate +
  new decision-table test).
- `just clippy` -- lint clean (new `match` arms exhaustive).
- `just check-output-ascii` and `just check-docs` -- docs/ASCII gates for the
  `balance-soft.md` edit.
- VM tests via `nix build .#checks.<system>.add-degraded-then-remove-missing` and
  `.#checks.<system>.braid-add-warnings` (registered as `flake.nix` `checks` entries).
