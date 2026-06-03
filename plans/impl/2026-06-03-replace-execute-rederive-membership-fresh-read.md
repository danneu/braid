# Plan: re-derive ReplacePlan::execute membership from a fresh read (mirror remove)

## Context

Follow-up from the committed remove-guard docs change (`docs(remove): document
why execute re-checks pool membership`, 8286bcac). It asked whether
`ReplacePlan::execute` needs the same execute-time pool.json re-check that
`RemovePlan::execute` has.

Investigation confirmed a real structural asymmetry:

- `RemoveWorkPlan` stores no membership; `RemovePlan::execute`
  (`cli/src/remove.rs#RemovePlan::execute`) re-loads pool.json fresh, rejects if
  the target drifted out (`absent_from_membership_error`), and re-derives
  `target_membership` from that fresh read -- pinned by
  `execute_rejects_when_pool_json_drifts_after_planning`.
- `ReplaceWorkPlan` stores `pre_membership` / `target_membership` as plan-time
  fields and `ReplacePlan::execute` (`cli/src/replace.rs#ReplacePlan::execute`)
  journals (`journal::build_journal`) and saves (`membership::save_membership`)
  from those stale snapshots, with no execute-time re-read. Its execute-time
  gates (`verify_replace_execute_live_pool_uuid`,
  `probe_existing_luks_new_target_uuid`) probe *live* btrfs/cryptsetup only,
  never pool.json.

We first planned a clarifying *comment* documenting the asymmetry as intentional.
Review killed that approach: the comment's safety argument was subtle enough to
get wrong on the first draft (it over-claimed "btrfs membership cannot change
under the held lock" -- false, replace itself mutates it -- and "save re-syncs
pool.json to live topology" -- false, `enrich_from_pool_state`
(`cli/src/membership.rs#enrich_from_pool_state`) refreshes only `devid`/`added_at`
of already-present members and does not reconcile the set). A fragile,
load-bearing comment in a mutation-critical function is exactly what braid's
safe-by-construction ethos says to replace with structure.

Decision: pivot to the behavioral mirror. Make `ReplacePlan::execute` re-derive
membership from a fresh pool.json read with a drift guard, identical in shape to
remove. This removes the asymmetry, makes the behavior self-documenting, and is
pinned by a test instead of a comment. Note this is a consistency/robustness win,
not a fix for a live race: the pool operation lock
(`docs/design/decisions/026-pool-lock-rust-owned.md`, exclusive non-blocking
flock held across plan->execute via `_pool_guard` in `cli/src/main.rs#main`)
already excludes concurrent braid writers; the guard hardens against a manual
mid-window pool.json edit and, more importantly, eliminates the stale-snapshot
class of bug at the source.

## Change

All edits in `cli/src/replace.rs`. No production behavior outside `replace`.

### 1. Stop storing membership on the plan

Remove `pre_membership` and `target_membership` from `ReplaceWorkPlan` (the
struct) and from `ReplaceWorkPlanInput`, and drop their assignment in
`build_replace_work_plan`. Verified they are consumed *only* in
`ReplacePlan::execute` -- `preview()` / `render_steps()` (the dry-run path via
`cmd_replace`) never read them, so nothing else breaks. This makes
`ReplaceWorkPlan` mirror `RemoveWorkPlan`, which stores no membership.

### 2. Shared derivation helper

Extract the existing plan-time derivation into a helper so plan-time validation
and execute derive identically (no drift between the two sites):

```rust
/// Derive post-replace membership: drop `old_uuid`, insert the new member,
/// running PoolMembership::insert's four-axis uniqueness invariant. Shared by
/// plan_replace (early/dry-run rejection) and ReplacePlan::execute
/// (authoritative, against a fresh pool.json read) so both derive identically.
fn derive_replace_target_membership(
    pre_membership: &PoolMembership,
    old_uuid: &LuksUuid,
    new_uuid: &LuksUuid,
    new_name: &DiskName,
    new_by_id: &ByIdPath,
) -> Result<PoolMembership, MembershipError> {
    let mut target = pre_membership.clone();
    target.remove_by_uuid(old_uuid);
    target.insert(
        new_uuid.clone(),
        membership::DiskMember { name: new_name.clone(), by_id: new_by_id.clone(), devid: None, added_at: None },
    )?;
    Ok(target)
}
```

`plan_replace` keeps its `assert_new_uuid_unique` call, then calls this helper to
validate (its result is discarded -- it runs solely to surface four-axis
membership conflicts during `--dry-run` and before the confirmation prompt; the
authoritative derivation runs again in `execute`). This preserves all current
plan-time validation; no dry-run regression.

### 3. Execute-time re-load + drift guard + re-derive

In `ReplacePlan::execute`, replace the current plan-time-snapshot journal inputs.
Insert, after the sleep-inhibitor guard and before `journal::build_journal`
(mirroring remove's post-inhibitor / pre-journal placement):

```rust
// (Confirm/passphrase/inhibitor-window guard) Re-load pool.json and re-derive
// target_membership here, mirroring RemovePlan::execute: journaling/saving the
// plan-time snapshot would persist a stale membership if pool.json was rewritten
// during the confirmation/passphrase/inhibitor window. Reject if old drifted
// out; derive_replace_target_membership's insert re-runs the four-axis
// uniqueness invariant against the fresh read. Pinned by
// replace_execute_rejects_when_pool_json_drifts_after_planning.
let pre_membership = membership::load_membership(params.paths)
    .map_err(|e| ReplaceError::Validation(format!("failed to load pool membership: {e}")))?;
if pre_membership.by_uuid(&old_uuid).is_none() {
    return Err(absent_from_membership_error(old_name.as_str()));
}
let target_membership =
    derive_replace_target_membership(&pre_membership, &old_uuid, &new_uuid, &new_name, &new_by_id)?;
```

All inputs (`old_uuid`, `new_uuid`, `new_name`, `new_by_id`) are already
destructured `work_plan` fields. The fresh `pre_membership`/`target_membership`
then flow unchanged into the existing `build_journal`, the post-replace
`enrich_from_pool_state` + `save_membership`, and `rewrite_journal`.

- **Journal fidelity preserved:** `plan_replace` does not enrich `pre_membership`
  between `load_membership` and use (verified), so the execute-time load yields
  identical provenance (pool.json persisted devids). Do NOT add remove's
  devid-pinning loop -- replace does not currently pin, and this change keeps
  journal content shape identical, leaving recovery semantics untouched.
- **The insert `?`** propagates a drift-introduced membership conflict via the
  existing `MembershipError -> ReplaceError` conversion (the same `e.into()`
  used in `plan_replace`); confirm that `From` impl exists and reuse it.

### 4. Drift-error helper

Add a `replace.rs`-local helper mirroring remove's, returning the same wording.
It is a new top-level fn, so it carries a `///` doc comment (AGENTS.md "Doc
Comments"), matching the doc comment on remove's `absent_from_membership_error`:

```rust
/// replace's execute-time `by_uuid` drift error -- same operator wording as
/// remove's `absent_from_membership_error`, so the two commands reject an
/// absent member identically before journaling.
fn absent_from_membership_error(name: &str) -> ReplaceError {
    ReplaceError::Validation(format!(
        "'{name}' not found in pool.json membership -- \
         no disk entry has this name. Pool membership may need manual repair."
    ))
}
```

(`replace`'s existing `ReplaceError::OldMemberNotFound` is the plan-time `by_name`
rejection; keep it there. The execute guard is a `by_uuid` re-check, so it uses
the remove-style `Validation` helper for byte-identical operator wording across
the two commands.)

## Test

Add `replace_execute_rejects_when_pool_json_drifts_after_planning`, mirroring
`cli/src/remove.rs#execute_rejects_when_pool_json_drifts_after_planning`. Model
the setup on the existing `execute_rechecks_live_pool_allows_clean_pool_before_journal`
fixture path (it reaches post-inhibitor/journal). After a successful
`plan_replace`/plan build, drift pool.json on disk (remove `old`'s entry via
`save_membership`), then run `execute` and assert:

1. `Err(ReplaceError::Validation(msg))` with `"not found in pool.json membership"`
   and the old disk name.
2. No journal written (`!paths.pending_op_json().exists()`) -- guard precedes
   `build_journal`.
3. Inhibitor acquired exactly once -- guard runs after the inhibitor (drift edit
   does not affect the earlier passphrase-verify / live-pool gates, which read
   plan-time `pool` and live probes, not pool.json).
4. Drifted pool.json left unchanged.

Test preamble follows the Intent / Why it exists / Scenario convention
(AGENTS.md "Test Conventions"). This is a Rust unit test; no VM test needed.

Add a second drift variant, `replace_execute_rejects_when_pool_json_drift_conflicts_with_new_disk`,
pinning the *other* new rejection path -- the helper's `insert ?`. §3 claims a
window edit that collides with the disk being added fails closed pre-journal;
without a test, an implementer could drop the `?` (or swallow the error) and
silently regress it. Setup: after planning, drift pool.json to **keep `old`
present** (so the absent-name guard does not short-circuit) but add a member
colliding with `new_by_id` (or `new_name`/`new_uuid`). Run `execute` and assert
`Err(ReplaceError::Membership(_))` (the `#[from] MembershipError` variant,
`replace.rs:152`) with no journal written.

## Deliberately NOT doing

- **No devid-pinning at journal time** for replace (see Journal fidelity above) --
  out of scope and would alter recovery input shape.
- **No recovery-path change.** `ReplacePlan::execute` is called only by
  `cmd_replace`; recovery replays via `recover.rs`
  (`finish_uncommitted_replace_recovery` and friends) and never calls `execute`,
  so it is unaffected.
- **No new ADR.** This aligns `replace` to an existing execute-time-re-derive
  pattern; it does not introduce a new principle.

## Critical files

- `cli/src/replace.rs` -- the only production file changed: `ReplaceWorkPlan` /
  `ReplaceWorkPlanInput` / `build_replace_work_plan` (drop fields), `plan_replace`
  (call shared helper for validation), `ReplacePlan::execute` (re-load + guard +
  re-derive), new `derive_replace_target_membership` and
  `absent_from_membership_error` helpers, new drift test.
- Read-only anchors: `cli/src/remove.rs#RemovePlan::execute` and its
  `absent_from_membership_error` + `execute_rejects_when_pool_json_drifts_after_planning`
  (the pattern being mirrored); `cli/src/membership.rs` (`insert`'s four-axis
  invariant, `remove_by_uuid`, `enrich_from_pool_state`).

## Verification

Behavioral change -- run the Rust suite:

- `just test-rust` -- builds `braid-cli`; the new drift test must pass, and the
  existing `execute_rechecks_live_pool_*`, `cmd_replace_*`, and replace journal
  tests must stay green (they stage pool.json on disk, so execute's fresh load
  resolves to the same membership; assertions are on live-pool behavior, not on
  the dropped fields).
- Confirm the dropped fields leave no dangling references (compiler will catch);
  confirm the `MembershipError -> ReplaceError` `From` reused by the insert `?`
  exists.
- Grep `docs/` for any claim that `replace` journals/saves from a plan-time
  membership snapshot; update if present (expected: none -- docs do not go to
  this granularity).
- Optional broader safety: `just test-vm` for replace-related checks if the
  reviewer wants VM-level confirmation, though the change is unit-test-pinned and
  recovery is untouched.

## Implementation notes

- The second drift test (`replace_execute_rejects_when_pool_json_drift_conflicts_with_new_disk`)
  collides on the `by_id` axis specifically: the foreign drift member carries a
  distinct UUID (`4444...`) and a decoy `name` so only `new_by_id` collides,
  isolating axis 3 of `PoolMembership::insert`'s four-axis invariant. The plan
  left the axis choice open ("colliding with `new_by_id` (or `new_name`/`new_uuid`)").
