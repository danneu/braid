# Plan: complete the recover mount-membership contract in docs + code

## Context

`braid recover` chooses which membership to open and mount based on the journal
phase. The authoritative dispatch is `mount_membership_for_recover` in
`cli/src/recover.rs` (the `match` at ~`recover.rs:3691-3727`), backed by
`recovery_admission_membership` (~`recover.rs:3669-3689`). It has eight arms
feeding three sources:

- **pre-operation membership** -- non-bootstrap `Add::PoolMutation`,
  `RemoveMissing::PoolMutation`
- **committed target membership** -- every post-maintenance phase
  (`PostAddBalanceRaid1`, `PostRemoveMissingMaintenance`,
  `PostReplaceMaintenance`)
- **admission membership** (`pre` + target-only, per
  `recovery_admission_membership`) -- `Replace::PoolMutation`, **bootstrap**
  `Add::PoolMutation`, and plain `OpKind::Remove`

Two prose statements describe this dispatch and **both omit the bootstrap-add
and plain-`Remove` arms**, and both make the blanket claim "Add `PoolMutation`
-> pre-operation membership" that is actually *wrong* for a bootstrap add (empty
`pre` -> mounts the new disk via admission):

1. `docs/commands/recover.md` step 2 of "What happens under the hood"
   (operator-facing).
2. `docs/design/decisions/017-runtime-disk-membership.md` (Active ADR;
   architecture authority for runtime membership).

Separately, the dispatcher `mount_membership_for_recover` has **no `///`
comment**, while its sibling `recovery_admission_membership` does. A doc comment
above the `match` is the only artifact co-located with the code, so it is the
one that makes a future arm change *visibly* contradict the contract -- which is
the finding's core concern.

A prior review flagged the doc gap (Low / Project fit). This plan does the
*ideal* fix: complete all three artifacts and make them mutually consistent,
with the function comment as the drift-proof source of truth.

Out of scope (deliberate): collapsing ADR 017's enumeration into a thin pointer
to `recover.md` is a docs-consolidation judgment call (separate concern, larger
editorial surface) -- not bundled here. `principles.md` and
`recovery-scenarios.md` describe what the journal *records*, not the mount
*source*, and need no change.

## Verified facts (grounding the wording)

- `is_bootstrap_add()` = `Add` op with empty `pre_membership`
  (`journal.rs#Journal::is_bootstrap_add`). For bootstrap, admission =
  `pre` (empty) + target-only = **the new disk**.
- `OpKind::Remove { luks_uuid, name }` is single-phase, distinct from the
  phased `RemoveMissing`. Its target is a subset of `pre`, so admission =
  `pre` + (no target-only) = **the pre-removal set**.
- No test asserts on the literal text of either doc (no golden/snapshot/grep
  test) -- editing both sentences is safe. `mdbook-linkcheck2` only validates
  links; none change.

## Changes

### 1. `docs/commands/recover.md` -- step 2 (currently one paragraph)

Replace the current step-2 body:

> Chooses the mount membership from the journal phase. Add and remove-missing
> `PoolMutation` phases mount from the pre-operation membership. Add,
> remove-missing, and replace post-maintenance phases mount from the committed
> target membership. Replace `PoolMutation` uses the pre/target union because
> the kernel may still be completing `dev_replace`.

with (carves bootstrap out of the "Add -> pre" rule, adds `Remove`, unifies on
the "admission membership" term already used in this doc's Safety-checks bullet):

> Chooses the mount membership from the journal phase. Existing-pool add and
> remove-missing `PoolMutation` phases mount from the pre-operation membership.
> Add, remove-missing, and replace post-maintenance phases mount from the
> committed target membership. Replace `PoolMutation`, bootstrap add
> `PoolMutation` (the first disk, whose pre-operation membership is empty), and
> `Remove` mount from the admission membership (pre-operation snapshot plus
> target-only members) -- for replace this matters because the kernel may still
> be completing `dev_replace`.

### 2. `docs/design/decisions/017-runtime-disk-membership.md` -- the "Mount membership is phase-specific:" sentence (~line 80)

Replace the sentence:

> Mount membership is phase-specific: add/remove-missing pool-mutation phases
> mount from the pre-operation membership, add/remove-missing post phases mount
> from the committed target membership, replace pool-mutation recovery uses the
> pre/target union, and replace post-maintenance recovery mounts from the
> committed target membership.

with (same content, ADR's dense compound style; leaves the rest of the long
paragraph untouched):

> Mount membership is phase-specific: existing-pool add and remove-missing
> pool-mutation phases mount from the pre-operation membership, add/remove-missing
> post phases and replace post-maintenance recovery mount from the committed
> target membership, and replace pool-mutation, bootstrap-add pool-mutation
> (empty pre-operation snapshot), and plain `remove` recovery mount from the
> admission membership (pre-operation snapshot plus target-only members, which
> for replace covers an in-flight `dev_replace`).

### 3. `cli/src/recover.rs` -- add `///` to `mount_membership_for_recover`

Add a doc comment directly above `fn mount_membership_for_recover<'a>(`
(~`recover.rs:3691`), mirroring the existing `recovery_admission_membership`
comment style and explaining the *why* for the three admission arms:

```rust
/// Phase-specific mount source: which membership recover opens and mounts
/// before probing live topology. Three sources, so an interrupted mutation
/// mounts the set still observable at its journal phase.
///
/// - Pre-operation membership: existing-pool `Add::PoolMutation`,
///   `RemoveMissing::PoolMutation`.
/// - Committed target membership: every post-maintenance phase
///   (`PostAddBalanceRaid1`, `PostRemoveMissingMaintenance`,
///   `PostReplaceMaintenance`).
/// - Admission membership (pre + target-only; see
///   `recovery_admission_membership`): `Replace::PoolMutation` (kernel may
///   still be finishing `dev_replace`), bootstrap `Add::PoolMutation` (pre is
///   empty, so this is the new disk), and plain `Remove` (target is a subset
///   of pre, so this is the pre-removal set).
```

## Critical files

- `docs/commands/recover.md` (step 2 paragraph)
- `docs/design/decisions/017-runtime-disk-membership.md` (Recovery bullet, mount
  sentence)
- `cli/src/recover.rs` (`///` on `mount_membership_for_recover`; reference
  `recovery_admission_membership`'s existing comment for tone)

## Verification

1. **Contract cross-check (primary):** read the eight arms of
   `mount_membership_for_recover` (`recover.rs:3695-3727`) and confirm each arm
   appears, with the correct source, in all three edited artifacts. Confirm no
   arm is described by more than one source.
2. **No stale phrasing lingers:** `rg -n "pre/target union"` and
   `rg -n "Add and remove-missing .PoolMutation. phases mount"` return nothing
   in `docs/`.
3. **Docs build / linkcheck:** `mdbook build docs` succeeds (no links changed;
   confirms the tree still builds).
4. **Comment compiles:** `just test-rust` (or `cargo build -p braid-cli`)
   succeeds -- the `///` is the only code change and must not break the build.
5. **House style:** edited lines use ASCII `--` (not em-dash) and backticked
   type/phase names, consistent with surrounding prose.

No behavioral change; no VM tests required.
