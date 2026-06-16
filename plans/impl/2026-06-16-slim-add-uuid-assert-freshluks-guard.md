# Slim `add`'s UUID-uniqueness assert; give FreshLuks its own live-pool guard

## Context

`braid add`'s pre-journal-write UUID checks live in `cli/src/add.rs`. Two
distinct collision concerns are entangled in one function,
`add.rs#assert_target_uuid_unique`:

- **Identity collisions** -- a target's UUID matching another in-flight add
  target or a persisted membership key. Both parties are real braid-resolved
  identities; the refusal `AddError::DuplicateUuid` names both.
- **Live-pool collisions** -- a target's UUID matching a device already live in
  the btrfs pool but absent from membership (a foreign/cloned disk). Per ADR 024
  braid invents no identity for it; the refusal `AddError::DuplicateUuidLivePool`
  names only the add target and reports by scope.

The live-pool concern is handled in **two** places today: the backing-aware
`add.rs#classify_live_pool_match` (called by both `PresentLuks` arms before the
assert) **and** arm (2b) inside `assert_target_uuid_unique` (a plain
`pool.devices` scan). For `PresentLuks` callers, classify has already run and
returned `NoMatch`, so arm (2b) is provably dead. Only the `FreshLuks`
(`PresentNotLuks`) arm -- which never calls classify -- relies on arm (2b). That
asymmetry forced a ~24-line explanatory doc/inline comment (added in
`plans/impl/2026-06-03-add-arm-2b-residual-backstop-docs.md`) and keeps
generating "why is this arm dead?" review findings.

The ideal fix is structural, not a comment trim: **separate the two concerns.**
`assert_target_uuid_unique` becomes identity-collisions only (in-flight +
membership), and `FreshLuks` gets its own right-sized live-pool guard. A freshly
generated `new_v4()` UUID has no legitimate same-backing live match (unlike a
returned `PresentLuks` disk), so it needs a plain `pool.devices` scan, not
backing-aware classification. Outcome: each function does one thing, no dead
arms, no asymmetry to explain, and the assert's now-unused `live_pool` parameter
drops out -- concrete proof the responsibility moved. This delivers the
`2026-06-03` plan's stated goal ("make the code self-documenting about which gate
owns live-pool clone rejection") through structure rather than prose, and keeps
the FreshLuks guard explicit and local -- harder to silently strip than an arm
buried in a shared helper.

Behavior is unchanged: every reachable live-pool refusal still produces
`DuplicateUuidLivePool` with the identical message (both old arm (2b) and the new
guard route through `add.rs#duplicate_live_pool_uuid_error`), and the FreshLuks
path preserves the old precedence -- identity scopes (in-flight + membership) run
before the live-pool guard, so a generated UUID that collides with a known member
still reports the informative `DuplicateUuid` naming both parties, not the
scope-only `DuplicateUuidLivePool`.

## Approach

### 1. Extract FreshLuks's live-pool guard (new helper)

Add a small, named, directly-testable helper in `cli/src/add.rs`, reusing the
existing `duplicate_live_pool_uuid_error` so the variant/message is byte-identical
to today's arm (2b):

```rust
/// FreshLuks's plan-time live-pool guard. A freshly-generated `new_v4()`
/// UUID has no legitimate same-backing live match (unlike a returned
/// PresentLuks disk), so a plain pool.devices scan is the right-sized check --
/// backing-path classification is unnecessary. Fail closed on the
/// astronomically unlikely collision before journal write; refuse by scope
/// per ADR 024 (no invented identity for the foreign device).
fn assert_fresh_uuid_absent_from_live_pool(
    uuid: &LuksUuid,
    live_pool: &PoolState,
    name: &DiskName,
    by_id: &ByIdPath,
) -> Result<(), AddError> {
    if live_pool.devices.iter().any(|d| d.luks_uuid == *uuid) {
        return Err(duplicate_live_pool_uuid_error(uuid, name, by_id));
    }
    Ok(())
}
```

### 2. Slim `assert_target_uuid_unique`

- Drop the `live_pool: &PoolState` parameter.
- Delete arm (2b) (the `live_pool.devices.iter().any(...)` block + its inline
  comment).
- Keep arm (1) in-flight and arm (2a) membership unchanged.
- Rewrite the doc-comment to describe only identity-collisions. Delete the "Arm
  (2b) is a fail-closed residual backstop ..." paragraph and the arm-2b bullet.
  State that live-pool collisions are owned by the per-caller gates
  (`classify_live_pool_match` for `PresentLuks`,
  `assert_fresh_uuid_absent_from_live_pool` for `FreshLuks`). Remove the
  now-stale citation of the `2026-05-12` plan's "Gate ordering vs in-flight
  targets map" section.

### 3. Wire the FreshLuks arm + drop the dropped arg at all call sites

In `add.rs#build_add_work_plan`'s `PresentNotLuks` arm, run the identity scopes
**before** the live-pool guard, preserving the old single-function precedence
(in-flight -> membership -> live-pool). The old `assert_target_uuid_unique`
checked membership (arm 2a, `DuplicateUuid`) before live-pool (arm 2b,
`DuplicateUuidLivePool`), so a generated UUID that collides with a device that is
both a known member and live must still surface the informative `DuplicateUuid`
naming both real identities -- never the scope-only `DuplicateUuidLivePool`:

```rust
// Identity scopes first (in-flight + membership), then the live-pool guard --
// the same order the old single assert used, so a collision with a known
// member still reports DuplicateUuid (both parties), not DuplicateUuidLivePool.
assert_target_uuid_unique(&luks_uuid, input.pool_membership,
                          &initial_journal_targets, name, by_id)?;
assert_fresh_uuid_absent_from_live_pool(&luks_uuid, input.pool, name, by_id)?;
```

Update the two `PresentLuks` call sites (open + closed arms) to drop the
`input.pool` argument from `assert_target_uuid_unique`. Their
`classify_live_pool_match` ordering is unchanged and must stay ahead of the
assert: its `SameBacking -> continue` case decides whether the target is planned
at all, so live-pool classification necessarily precedes the identity scopes on
the `PresentLuks` paths (a pre-existing asymmetry with FreshLuks, justified by
the returned-disk no-op case, not introduced here).

### 4. Repoint the one breaking test

`add.rs` test `assert_target_uuid_unique_live_pool_collision_omits_foreign_mapper`
directly drove arm (2b). Repoint it to call
`assert_fresh_uuid_absent_from_live_pool` with the same hand-crafted live pool;
**keep every assertion** (`DuplicateUuidLivePool` variant, names `braid-disk2`,
`"live pool"` scope, no `clone-foreign` leak). Rename + update its
Intent/Why/Scenario preamble to say it pins the FreshLuks-owned guard. Because
the repointed test uses an empty membership + empty in-flight map, it exercises
the live-pool guard in isolation and is unaffected by the step-3 ordering.

One more current-code comment needs updating: the comment block in
`add_live_pool_collision_omits_braid_prefixed_mapper` cross-references "arm (2b)"
and the old test name. Repoint it to the per-caller gates and the renamed
FreshLuks-guard test, so no dangling "arm (2b)" reference survives in
`cli/src/add.rs`.

All other tests pass unchanged (verified by full inventory): the
`classify_live_pool_match` units, `PresentLuks` open/closed integration
(`add_*_present_luks_same_uuid_*`), in-flight/membership integration
(`add_cloned_disk_duplicate_uuid_refusal`,
`add_pre_write_uniqueness_assert_membership_collision`), execute-recheck
(`execute_live_pool_recheck_rejects_different_backing`), and the
integration-level message contract via
`add_live_pool_collision_omits_braid_prefixed_mapper`. The
`DuplicateUuidLivePool` rendering contract therefore stays covered independently
of the removed arm.

### 5. Doc sync (light-touch)

- `cli/src/replace.rs#assert_new_uuid_unique` doc-comment: update the "Mirrors
  `add.rs::assert_target_uuid_unique`" line. The mirror is now **intentionally
  asymmetric**: `add` separates identity- and live-pool concerns (its live-pool
  check is caller-dependent -- classify vs scan), while `replace` keeps one
  uniform live-pool scan because its new disk is always distinct hardware (no
  same-backing-noop case). Leave `replace`'s **code** as-is -- its bundling is
  right-sized for replace's semantics.
- Add a one-line "Superseded by this change" breadcrumb to the top of
  `plans/impl/2026-06-03-add-arm-2b-residual-backstop-docs.md` and to the "Gate
  ordering vs in-flight targets map" section of
  `plans/impl/2026-05-12-luks-uuid-as-identity/plan.md` (historical plans --
  breadcrumb, not rewrite).
- ADR 024 (`docs/design/decisions/024-luks-uuid-identity.md`) needs no change --
  it pins the invariant, which this consolidation strengthens.
- **Flag for the user:** `plans/wip/plan-the-ideal-pivot-fix-composed-salamander.md`
  is an open WIP that assumes arm (2b) stays. Retire or reconcile it (your call);
  not edited by this plan.

## Critical files

- `cli/src/add.rs` -- new helper, slimmed assert (param + arm dropped),
  FreshLuks wiring, 3 call-site arg drops, doc-comments, 1 test repoint.
- `cli/src/replace.rs` -- mirror doc-comment touch-up only (no code change).
- `plans/impl/2026-06-03-add-arm-2b-residual-backstop-docs.md`,
  `plans/impl/2026-05-12-luks-uuid-as-identity/plan.md` -- superseded breadcrumbs.

## Reused existing code

- `add.rs#duplicate_live_pool_uuid_error` -- the new helper and all surviving
  callers emit through it, so `DuplicateUuidLivePool` stays the single
  live-pool-refusal producer.
- `add.rs#classify_live_pool_match` -- unchanged; remains the backing-aware
  gate for `PresentLuks` (plan-time) and for every target at execute time
  (`recheck_execute_live_pool_targets`).

## Verification

- `just test-rust` -- primary gate; the repointed test passes and nothing else
  regresses.
- `cargo build` + `cargo clippy` -- confirm the `live_pool` param drop compiles
  cleanly with no dead code/imports left behind.
- `rg -n 'arm \(?2b\)?|residual backstop' cli/src/add.rs cli/src/replace.rs` --
  confirm no dangling references to the removed arm survive in current code or
  comments. Scope is deliberately current code only: historical `plans/impl/`
  references (including the literal `2026-06-03-add-arm-2b-residual-backstop-docs.md`
  filename) are intentional point-in-time records and get breadcrumbs, not
  deletion, per step 5 -- grepping `plans/` here would contradict that decision.
- `rg -n 'Superseded' plans/impl/2026-06-03-add-arm-2b-residual-backstop-docs.md
  plans/impl/2026-05-12-luks-uuid-as-identity/plan.md` -- confirm both historical
  breadcrumbs were added.
- `rg -n 'assert_target_uuid_unique\(' cli/src/add.rs` -- confirm all three call
  sites dropped the pool argument.
- Existing Python/Nix VM add tests
  (`tests/cli/braid-add-cloned-luks-header-rejected.py`,
  `tests/cli/braid-add-uuid-swap-rejected.py`) remain a green sanity check --
  they exercise the classify/execute paths this change does not alter, so they
  are not a required target but should stay passing.

## Follow Up

- `plans/wip/plan-the-ideal-pivot-fix-composed-salamander.md` is an open WIP
  that assumes arm (2b) stays in `assert_target_uuid_unique`. Arm (2b) is now
  gone (split into `assert_fresh_uuid_absent_from_live_pool`), so that plan
  needs retiring or reconciling against the new structure. Left untouched by
  this change per step 5.
