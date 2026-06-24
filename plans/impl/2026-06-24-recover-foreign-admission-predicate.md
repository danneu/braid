# Plan: unify the foreign-admission predicate in recover.rs

## Context

An `/ultrareview` finding flagged that `execute_add_pool_mutation_recovery`
"validates foreign-live-device admission twice" and proposed *dropping* the
`validate_live_members_allowed` call that precedes the terminal
`build_membership_from_live_pool`.

Investigation (verify-issue) showed the finding's **fix is wrong**, but its
**root observation is real**:

- There is no standalone `validate` immediately before either terminal builder.
  The "terminal" call the finding targets is actually the **last statement of
  the per-target replay loop** (`recover.rs#execute_add_pool_mutation_recovery`,
  the `validate_live_members_allowed(&pool, union)?` inside the
  `for (target_uuid, target) in targets` loop). It runs after each replayed
  `pool_add_device` + re-probe and **gates before the next target's mutation**.
  In a multi-target add (exercised by `live_add_recovery_drops_ghosts_for_mixed_batch`)
  deleting it lets recovery add disks 2..N before noticing a foreign device --
  strictly more mutation before failing. That contradicts the fail-closed
  posture in [`safety-heuristics.md`](../../docs/dev/safety-heuristics.md). So
  **no gate gets dropped.**
- The genuine problem is duplication of the *predicate*: `validate_live_members_allowed`
  and `build_membership_from_live_pool` each independently encode
  `admission.by_uuid(&dev.luks_uuid)` -> `foreign_live_device_not_admitted(dev)`.
  A maintainer changing the admission *rule* could edit one and miss the other.

**Outcome:** give the foreign-admission rule a single source of truth by
extracting one helper that both functions call, with zero behavior change
(identical error, every gate preserved). Document the defense-in-depth gating
so this class of "looks redundant" finding stops recurring.

## Approach

### Part 1 -- extract the shared admission gate (the core change)

Add one private helper to `cli/src/recover.rs`, placed next to
`foreign_live_device_not_admitted` (it must live here, not in `membership.rs`,
because it returns `RecoverError`; `membership.rs` is the lower layer and its
existing `foreign_luks_uuids` filter returns a plain map for that reason):

```rust
/// Single foreign-admission gate for recovery rebuilds: resolve a live
/// device's phase-aware admission member or fail closed. Both the standalone
/// pre-mutation check and the membership builder funnel through this one
/// `by_uuid` lookup, so the admission rule has one definition and cannot drift
/// between them.
fn admitted_live_member<'a>(
    admission: &'a PoolMembership,
    dev: &PoolDevice,
) -> Result<&'a DiskMember, RecoverError> {
    admission
        .by_uuid(&dev.luks_uuid)
        .ok_or_else(|| foreign_live_device_not_admitted(dev))
}
```

Reuses existing pieces verbatim: `PoolMembership::by_uuid` (`cli/src/membership.rs`,
returns `Option<&DiskMember>`) and `foreign_live_device_not_admitted`
(`cli/src/recover.rs`). The explicit `'a` ties the returned `&DiskMember` to
`admission`; `dev` keeps its own elided lifetime.

Rewrite the two call sites to delegate -- no other lines change:

- `recover.rs#validate_live_members_allowed`: loop body becomes
  `admitted_live_member(allowed, dev)?;` (result discarded -- it stays a
  pure pass/fail gate).
- `recover.rs#build_membership_from_live_pool`: replace the
  `let Some(admission_member) = ... else { return Err(...) };` with
  `let admission_member = admitted_live_member(admission_membership, dev)?;`
  Everything downstream (`resolve_by_id_for_underlying`, `resolve_added_at`,
  the `DiskMember` insert) is untouched.

**Explicitly excluded from the unification** (verified, intentional divergence --
folding them in would regress real contracts):

- `recover.rs#recover_membership_matching_expected` -- same shape but uses
  *committed target* membership and a deliberately different error message
  ("not part of the expected committed membership"); its inline comment cites
  Decision 017 for the wording split.
- `membership.rs#foreign_luks_uuids` -- same predicate but *collects* foreign
  devices into a map instead of failing closed; different return type/purpose.
- `recover.rs#live_pool_matches_membership` -- *bidirectional* set-equality, a
  stronger check than the one-directional admission predicate.

### Part 2 -- document the defense-in-depth gating (kills the re-filing)

The finding's confusion was "why validate at entry, after re-probe, AND
per-target, when the builder checks the same thing?" Add a one- to two-line
comment at the per-target gate inside `execute_add_pool_mutation_recovery`
(the densest, cited site) making the rationale explicit, e.g.:

```rust
// Per-target fail-closed gate: a foreign device surfacing mid-batch stops
// further pool_add_device here rather than at the terminal builder. The
// builder (build_membership_from_live_pool) is the final admission gate;
// these standalone checks just fail earlier, before the next mutation.
```

Keep it scoped to the add path that the finding cited; the rationale generalizes
to the entry/post-re-probe/replace gates without repeating the comment.

## Files to modify

- `cli/src/recover.rs` -- add `admitted_live_member`; delegate from
  `validate_live_members_allowed` and `build_membership_from_live_pool`; add the
  Part 2 gating comment; add the mid-batch fail-closed regression test (Axis 2
  below) in `mod tests`, plus the btrfs-show fixture variants its probe sequence
  needs (a post-target1 state with the foreign device but *without* target2, and a
  post-target2 state with both targets and the foreign device) if no existing
  fixtures fit. No other files change.

## Test-coverage audit

Two axes, per the plan-review rubric: the **change** (Part 1 extraction) and the
**claim about existing behavior** (Part 2's load-bearing per-target gate).

**Axis 1 -- the extraction (behavior-preserving).** Same error string, every gate
preserved, so the existing tests are the right bar; both unified call sites are
already pinned independently:

- `recover.rs#build_membership_from_live_pool_rejects_foreign_live_uuid` --
  exercises the **builder** directly; asserts "recovery admission membership".
- `recover.rs#plan_recover_dry_run_already_mounted_rejects_foreign_live_uuid`
  and `recover.rs#plan_recover_post_replace_maintenance_rejects_live_old_uuid`
  -- exercise `validate_live_members_allowed` via `plan_recover`; same assertion.

If the helper drifts from either caller's old behavior, at least one breaks. No
direct unit test of `admitted_live_member` -- that would be structure-sensitive
(pinning an internal helper), which the project's bar discourages.

**Axis 2 -- the mid-batch fail-closed gate (NEW test -- the gap reviewer F1 caught).**
Part 2 elevates the per-target `validate_live_members_allowed` (the last
statement of the replay loop) to a load-bearing claim, and the original finding
proposed *deleting* it. No test pins that property: the three tests above reject
a foreign device at the builder or in the planner's pre-replay path -- none
exercises a foreign device **surfacing after one replayed add, before the next**.
Deleting the gate would still let the terminal builder fail at the end, so every
existing test stays green while an extra `BtrfsDeviceAdd` slips through. Add one
behavioral executor test to close this:

- **Harness:** call `execute_add_pool_mutation_recovery(...)` directly (as
  `recover.rs#live_add_recovery_drops_ghosts_for_mixed_batch` does) with a
  two-target add journal (`two_target_recoverable_pool_mutation_add_journal`) and
  the matching admission union (`test_recovery_admission_membership`). Entry
  `ctx.pool` = only the surviving disk, so **both** targets start missing and both
  get replayed. (This deliberately diverges from the precedent, whose entry
  `pool_state_disk1_and_disk2_devid4()` already has the first target live and so
  replays only one -- the two-missing entry pool is what creates the "foreign
  device surfacing *between* two replayed adds" window this test guards. Use a
  surviving-disk-only state, e.g. the precedent's `pool_state_disk1...` family.)
  Mock **each** target mapper's `BtrfsFilesystemShowTarget` to the no-btrfs output
  (`btrfs_show_target_no_btrfs`, exactly as the precedent does for `braid-disk3`)
  so `scan_mapper_if_btrfs_visible` returns false, `opened_or_scanned` stays false,
  and the post-open `probe_pool` is skipped -- making the per-target re-probes the
  *only* pool probes so the two-entry sequence below lines up exactly. (Verified:
  `verify_recover_passphrase_for_add_replay`, which runs before the replay loop,
  issues no `probe_pool`/`BtrfsFilesystemShow`, so it consumes no sequence entry.)
- **Staging:** drive the pool re-probes with
  `MockRunner::with_output_sequence(CmdRequest::BtrfsFilesystemShow { mount_point }, vec![...])`
  (mechanism confirmed in `cmd.rs#dispatch` -- sequences drain in order before the
  fixed output; precedent in `add.rs`, `unlock.rs`, `mapper_close.rs`). With
  `opened_or_scanned` false (see Harness), the only pool probes are the two
  per-target re-probes (`pool = probe::probe_pool(...)` right after each replayed
  `pool_add_device`; within this recovery path `probe_pool` is the only *reached*
  emitter of `BtrfsFilesystemShow { mount_point }` -- `probe_pool_alerts` and
  `probe_fsid` emit the same request but are not called from
  `execute_add_pool_mutation_recovery`), so a **two-entry** sequence drives the
  whole run. Each entry must model its post-add pool state distinctly -- a single
  fixed "foreign-present" output cannot, because the per-target loop both re-checks
  `live_member_uuids(&pool)` at the top of each iteration *and* requires the
  just-added target to appear via `pool.device_by_uuid(target_uuid)`
  (`types.rs#device_by_uuid`; else `AckCleanupFailed`):
    0. **Post-target1 re-probe:** surviving + target1 + foreign, **no target2**.
       target1 present so `device_by_uuid` succeeds; foreign present so the
       per-target gate fires here in the green run. target2 *absent* is the load-
       bearing detail -- it keeps the gate-deleted run from `continue`-skipping
       target2 at the next iteration's `live_member_uuids` check, so that run
       actually issues target2's `pool_add_device`.
    1. **Post-target2 re-probe** (reached only in the gate-deleted run): surviving
       + target1 + target2 + foreign. target2 now present so `device_by_uuid`
       succeeds (no `AckCleanupFailed` -- the failure mode if entry 0 were reused);
       foreign still present so the terminal `build_membership_from_live_pool` --
       which consumes this same `pool`, it does not re-probe -- fails closed with
       the admission error.
  Set the **fixed fallback** output (returned once the two-entry sequence empties)
  to the all-present state (surviving + target1 + target2 + foreign) as a safety
  net for any probe beyond the two modeled. The foreign UUID must not be in the
  admission union. The green run consumes entry 0 and the gate fires right after
  post-target1; the gate-deleted run consumes entries 0-1 and hands entry 1's pool
  to the builder. This distinct-entries sequencing is what makes "assertion 3 only"
  achievable in the red-first proof -- a single fixed output forces either an early
  `AckCleanupFailed` (assertion 1 also reds) or a `continue`-skipped target2
  (assertion 3 never reds). Build the two foreign-bearing pool states from the
  precedent's `with_disk..._pool_probe` helper family.
- **Assertions (all observable / structure-insensitive):**
  1. `Err` whose message contains "recovery admission membership".
  2. `f.paths.pending_op_json().exists()` -- journal preserved for the operator.
  3. `runner.requests()` holds exactly one `CmdRequest::BtrfsDeviceAdd`
     (target1's), and none for target2's mapper -- the second mutation never
     issued. **This is the load-bearing assertion for the per-target gate.**
- **Which assertions discriminate the gate:** assertions 1 and 2 hold *whether or
  not* the per-target gate exists, because the terminal `build_membership_from_live_pool`
  also fails closed on the foreign device (identical `foreign_live_device_not_admitted`
  error) and returns *before* the journal-clearing `write_add_phase` /
  `execute_add_post_balance_recovery` ever runs -- so with the gate deleted the
  same error and the same preserved `pending-op.json` are produced by the builder
  instead. Only assertion 3 (target2's `BtrfsDeviceAdd` suppressed) distinguishes
  gate-present from gate-absent; assertions 1-2 are green-path contract checks that
  document the fail-closed outcome. (This is why the red-first proof targets
  assertion 3 alone -- see Verification.)
- **Preamble:** the required `// Intent / Why it exists / Scenario` block naming
  the regression -- an interrupted multi-target add where a stray device joins
  the btrfs pool mid-replay must stop before adding the remaining targets.

This mirrors the precedent `recover.rs#recover_fails_when_device_missing_from_both_snapshots`,
which pins an error/mutation contract so a future "looks redundant" edit cannot
silently regress it.

## Verification

1. `just test-rust` (or `cargo test` in `cli/`) -- full suite green, with
   particular attention to the three Axis-1 foreign-rejection tests, the new
   Axis-2 mid-batch test, and the precedent guard
   `recover_fails_when_device_missing_from_both_snapshots`.
2. **Red-first proof for the Axis-2 regression guard** (the gate already exists,
   so the new test passes against current code -- prove it actually guards the
   gate): temporarily delete the per-target `validate_live_members_allowed` at
   the end of the replay loop and confirm the new test goes RED on **assertion 3
   only** -- target2's `BtrfsDeviceAdd` is now issued before the terminal builder
   finally rejects the foreign device. Assertions 1 and 2 stay GREEN even with the
   gate gone: `build_membership_from_live_pool` fails closed on the same predicate
   and returns its error *before* `write_add_phase` / `execute_add_post_balance_recovery`
   would clear the journal, so the admission-membership error and the preserved
   `pending-op.json` come from the terminal builder, not the deleted gate. Restore
   the gate after. The discriminating delta is the suppressed second mutation --
   exactly the fail-closed property Part 2 documents.
3. `cargo clippy` in `cli/` -- confirm the `'a` lifetime and `ok_or_else`
   closure compile clean with no new warnings.
4. Sanity-grep that `foreign_live_device_not_admitted` now has exactly one
   caller path through `admitted_live_member` (both former inline sites gone),
   confirming the single-source-of-truth goal is actually met.
