# remove-missing: thread the planner's membership snapshot into execute

## Context

`RemoveMissingPlan::execute` reloads `pool.json` membership and rebuilds
`target_membership` even though `plan_remove_missing` already loaded
membership and resolved the target UUID/name into the work plan. This is
the residue of a deleted drift recheck: commit `33ba9ee`
("refactor(remove-missing): drop pre-mutation devid drift recheck")
removed the re-resolve-and-abort logic from this exact spot once the pool
lock began covering the whole `cmd_remove_missing` lifetime, but kept the
reload, reasoning only "the journal builder and `target_membership` still
need it" -- it never asked whether the planner's snapshot could serve
instead.

Problems with the leftover reload:

- **Two membership reads per real run**, and two places that must agree on
  what "the membership at operation start" means (the planner resolves the
  target against load #1; the journal anchors its `pre_membership` on
  load #2).
- **`target_membership.remove_by_uuid(&target_uuid)` discards its
  `Option<DiskMember>` return** (`remove_missing.rs:213`). An execute-time
  membership that no longer contained `target_uuid` would silently produce
  a no-op removal and persist it, with no assertion that the removal
  matched -- a latent gap that only the pool lock currently makes
  unreachable.

The reload returns identical bytes to the planner's snapshot because the
pool lock (`_pool_guard`, `main.rs:493`, acquired before the
`match cli.command` dispatch and held for the whole command;
`RemoveMissing` real-run policy is `NonBlocking`, so the guard is held)
guarantees `pool.json` cannot change between plan and execute. This is the
same invariant `33ba9ee` relied on to delete the drift recheck.

**Outcome:** have the planner compute and store both membership snapshots;
have `execute` consume them; delete the reload. Behavior is unchanged
(provably identical bytes under the lock). The discarded-`Option` gap
disappears structurally, because `target_membership` is built in the
planner immediately after `resolve_removal_target` confirmed
`target_uuid` is present in that exact snapshot -- the removal is a
tautology there, so no defensive assertion is introduced.

This converges `remove_missing` onto the pattern its siblings already use:
`add.rs` stores the planner's `pool_membership` on the outer `AddPlan`
(`add.rs:941`) and builds `target_membership` from it without reloading;
`replace.rs` stores both `pre_membership` and `target_membership` computed
in the planner (`replace.rs:235-236`, `1426-1448`) and consumes them in
`execute` with no reload (`replace.rs:425-426`). After this change
`remove.rs` is the only mutator that reloads in `execute` -- and it
legitimately does so, because it additionally re-pins live devids and
asserts target presence (`remove.rs:331-345`); leave it untouched.

## The fix

All edits in `cli/src/remove_missing.rs`.

1. **Add two fields to `RemoveMissingPlan`** (the outer struct, currently
   `{ pub notes, work_plan }` at `:87-90`), matching `add.rs`'s
   outer-struct placement -- NOT the inner `RemoveMissingWorkPlan`:

   ```rust
   pre_membership: membership::PoolMembership,
   target_membership: membership::PoolMembership,
   ```

   `RemoveMissingPlan` derives nothing, so no derive change is needed.
   `PoolMembership` is `Clone + Debug` (`membership.rs:221`). Put the
   snapshots on the outer struct -- do NOT add them to the inner
   `#[derive(Clone)]` `RemoveMissingWorkPlan`: its `render_steps` /
   dry-run `preview()` never needs membership, and adding them would force
   `remove_missing_work_plan_for_test` and its call sites to synthesize
   snapshots. (The inner work-plan struct's *only* change is the separate
   removal of the now-dead `target_uuid` field -- step 4.)

2. **Update the `RemoveMissingPlan` doc comment** (`:80-90`) to note it now
   carries the pre-resolved `pre_membership` / `target_membership`
   snapshots that `execute` consumes (the journal's before/after and the
   persisted membership), replacing the current "reload at execute"
   framing.

3. **In `plan_remove_missing`**, right after `target_uuid` is resolved
   (`:440-444`) and before/at the `Ok(RemoveMissingPlan { ... })`
   construction (`:468`), build the target snapshot from the already-loaded
   `pre_membership`:

   ```rust
   // target_uuid was just resolved from pre_membership via by_devid, so
   // the removal is guaranteed to match -- no drift check needed (the pool
   // lock pins pool.json for the whole command; see decision 026).
   let mut target_membership = pre_membership.clone();
   target_membership.remove_by_uuid(&target_uuid);
   ```

   Then store both `pre_membership` and `target_membership` in the returned
   `RemoveMissingPlan`. (`pre_membership` is currently dropped at the end of
   the planner; now it is moved into the plan.) `remove_by_uuid` borrows
   `target_uuid`, so the local survives the call -- but it is now consumed
   *only* here and is no longer stored on the work plan (step 4 drops the
   `target_uuid,` entry from the `RemoveMissingWorkPlan { ... }`
   construction).

4. **Remove the now-dead `target_uuid` field from `RemoveMissingWorkPlan`.**
   Once the planner builds the target snapshot, nothing reads
   `work_plan.target_uuid` (its sole reader was the deleted execute block),
   so leaving it would be write-only dead state. Delete it in all four
   places:
   - the field declaration on `RemoveMissingWorkPlan` (`:95`);
   - the `target_uuid,` shorthand in the planner's work-plan construction
     (`:462`);
   - the `target_uuid: LuksUuid::parse(...)` initializer in the test
     constructor `remove_missing_work_plan_for_test` (`:592`);
   - the `let target_uuid = work_plan.target_uuid.clone();` local in
     `execute` (`:168`).

   `target_name` is NOT dead and stays put: `execute` reads it at `:169`
   for the confirmation message. The `LuksUuid` import stays too -- still
   used by `resolve_removal_target` and the test module.

5. **In `RemoveMissingPlan::execute`**:
   - Extend the destructure at `:163-166` to bind the new fields; keep
     `work_plan` bound whole (execute reads ~8 `work_plan.*` fields -- do
     not destructure it):
     ```rust
     let RemoveMissingPlan { notes: _, work_plan, pre_membership, target_membership } = self;
     ```
   - Delete the `target_uuid` local at `:168` (per step 4) and the reload +
     rebuild block at `:206-213` (the `load_membership` call, the
     `pre_membership` rebind, the clone, and the `remove_by_uuid` line).
   - Feed the bound snapshots into the existing journal/save calls
     unchanged in shape: `journal::build_journal(pre_membership,
     target_membership.clone(), ...)` (`:215-223`) and
     `save_membership(&target_membership, ...)` (`:257`). `build_journal`
     already takes both by value / clone, so the move of `pre_membership`
     and the surviving `&target_membership` borrow compile without a
     partial-move conflict.

6. **One forced test-literal edit:** `plan_preview_renders_warn_above_steps`
   (`:2135`) constructs `RemoveMissingPlan { notes, work_plan }` directly.
   Add `pre_membership: PoolMembership::empty(), target_membership:
   PoolMembership::empty(),` -- the test only calls `preview()`, so the
   values are irrelevant. Omitting this is a compile error, not a silent
   bug.

No assertion on the planner-side `remove_by_uuid` return: it is a
same-snapshot tautology, not a guard against external state, so an
`assert!`/`expect!`/`debug_assert!` there would be unreachable dead code.
(This is distinct from AGENTS.md's "residual invariant checks must be hard
errors" rule, which is about guards against real drift/corruption.)

## Tests

The final persisted `pool.json` is already pinned by structure-insensitive,
command-level (`cmd_remove_missing`, full plan -> execute) tests that reload
the saved membership and assert the result:

- `cmd_remove_missing_resolves_devid_to_uuid_and_issues_no_luks_uuid_probes`
  (`:2485`) -- target UUID gone, exactly the survivors remain after a real
  run.
- `cmd_remove_missing_decoy_regression_selects_by_devid_only` (`:2543`) --
  the by-devid-selected member is removed and a decoy entry is byte-stable.
- `cmd_remove_missing_prunes_acked_stats_for_removed_devid` (`:1337`) and the
  never-enriched / preflight refusal tests -- unaffected.

These catch a wrong `target_membership` on the success path (it is exactly
what `save_membership` writes). They do NOT observe the journal's
`pre_membership` / `target_membership` snapshots -- the recovery contract
`braid recover` replays. On success the journal is cleared, and
`pre_membership` is never written to `pool.json` at all, so no pool.json
assertion can catch a threading miswire in
`build_journal(pre_membership, target_membership.clone(), ...)`. Since this
refactor moves production of both snapshots into the planner, add one
assertion to close that gap:

- **Extend `journal_survives_device_remove_failure` (`:1621`).** This is the
  device-remove-failure path, where `write_journal` has run but
  `save_membership` has not, so the surviving `pending-op.json` is the *only*
  artifact of both snapshots. After the existing `load_journal(...).is_some()`
  check (`:1646`), load the journal and assert `journal.pre_membership`
  contains disk1/disk2/disk3 while `journal.target_membership` contains only
  disk1/disk2 (both fields are `pub`; assert via `PoolMembership::by_name`
  over the `three_disk_devids_pinned` fixture). This is behavioral (the
  recovery before/after) and structure-insensitive (membership names in the
  persisted journal, not internal wiring), and it catches a pre/target swap
  or wrong-snapshot thread that the pool.json tests cannot.

No other new coverage is needed. The drift test that *did* exercise an
execute-time reload was already deleted in `33ba9ee`.

## Verification

- `just test-rust` -- the Rust unit suite. Expect: clean compile (the one
  test literal updated), and all `remove_missing` tests green
  (positive path, decoy regression, acked-stats prune, never-enriched
  refusals, preflight rejects, work-plan render tests, and the three
  journal-survival tests -- including the extended
  `journal_survives_device_remove_failure` snapshot assertions). Confirm a clean
  compile with no new `unused`/`dead_code` warnings -- in particular that
  the `target_uuid` removal leaves nothing write-only and that the new
  `pre_membership`/`target_membership` plan fields are both read.
- `just test-vm braid-remove-disk` -- the lifecycle VM check that exercises
  `braid remove-missing` end-to-end (dead-disk remove-missing, `pool.json`
  pruning, LUKS cleanup). This is the production execute path the reload
  lived on. Scope is small; do not run the full `just test-vm` unless this
  check fails in a way suggesting broader impact.
- No fixture refresh: no parser-critical tool versions are touched.
