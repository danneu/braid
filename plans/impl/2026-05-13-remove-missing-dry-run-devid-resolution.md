# Fix: `braid remove-missing --dry-run` misses the never-enriched / no-member-for-devid refusal

## Context

`braid remove-missing` resolves the operator's `--missing-id <devid>` to a
persisted `(LuksUuid, DiskName)` via `PoolMembership::by_devid`. When no
member in `pool.json` has that devid enriched (enrichment never ran for
that member, the operator passed a foreign devid, or membership is in
the recovery state described in
`docs/decisions/024-luks-uuid-identity.md`), the resolution returns
`RemoveMissingError::NoMemberForDevid` with the pinned wording
`no member in membership has devid {devid}`.

Today that resolution lives only inside `RemoveMissingPlan::execute`
(`cli/src/remove_missing.rs:176-180`). The dry-run gate short-circuits
before execute (`cli/src/remove_missing.rs:479-482`), so
`braid remove-missing --dry-run --missing-id <devid>` happily renders a
"successful" preview and exits 0 on inputs that a real run would refuse.

This violates the dry-run contract pinned by
`docs/decisions/022-dry-run-preview-model.md`:

> `plan_*()` owns everything above the dry-run gate: config/state
> loading, preflight checks, live probes, accumulated preview notes,
> and construction of a typed work plan.

The sibling commands already do it right:

- `cli/src/remove.rs:530-543` -- `plan_remove` loads membership and
  resolves the user-typed name to a `(UUID, name)` pair.
- `cli/src/replace.rs:1113-1149` -- `plan_replace` loads membership and
  resolves `--old <name>` to a UUID.

`remove-missing` is the outlier left over from the LUKS-UUID-identity
migration. The fix moves the resolution into the planner and pins the
dry-run behavior with a regression test.

## Root cause

`cli/src/remove_missing.rs:176-180` -- `load_membership` +
`resolve_removal_target` run inside `execute`, after the dry-run gate.
`plan_remove_missing` (`cli/src/remove_missing.rs:319-459`) never
touches `pool.json` membership and therefore cannot surface
`NoMemberForDevid` from a dry-run preview.

## Fix

The work all lives in `cli/src/remove_missing.rs`. Four mechanical
edits.

### 1. Extend `RemoveMissingWorkPlan` with resolved identity

Add the two semantic decisions the planner now makes:

```rust
#[derive(Debug, Clone)]
struct RemoveMissingWorkPlan {
    missing_id: u64,
    target_uuid: crate::types::LuksUuid,
    target_name: crate::types::DiskName,
    will_clear_last_missing: bool,
    remaining_present: usize,
    missing_count: u64,
    mount_point: MountPoint,
}
```

Reason: `target_uuid` and `target_name` are semantic choices made
during planning; per doc 022 they belong on the work plan so
`execute()` does not rediscover them. This mirrors `RemoveWorkPlan`
in `cli/src/remove.rs`, which stores resolved `target_uuid` /
`target_name` (`cli/src/remove.rs:75-90`).

### 2. Resolve identity in `plan_remove_missing`

Insert immediately AFTER the 2-disk RAID1 hard reject at
`cli/src/remove_missing.rs:413-428`, BEFORE the expensive
`check_relocation_space` probe at lines 437-447:

```rust
// pool.json membership resolution. Run here so --dry-run sees the
// same NoMemberForDevid refusal as a real run (doc 022 contract).
// Cheap fs read; placed before the expensive relocation-space probe
// so we fail fast when membership cannot identify the target.
let pre_membership = match membership::load_membership(params.paths) {
    Ok(m) => m,
    Err(e) => {
        return Err(PlanFailure::with_notes(
            notes,
            RemoveMissingError::Validation(format!(
                "failed to load pool membership: {e}"
            )),
        ));
    }
};
let (target_uuid, target_name) =
    match resolve_removal_target(params.missing_id, &pre_membership) {
        Ok(p) => p,
        Err(e) => return Err(PlanFailure::with_notes(notes, e)),
    };
```

Placement rationale, in order from earliest to latest preflight:

1. AFTER the cheap live-state checks (`missing_count`,
   `devid-is-live`, `missing_devids.contains`) so the existing
   pool-shape errors still fire first.
2. AFTER the 2-disk RAID1 hard reject. That reject carries the
   documented repair guidance from
   `docs/decisions/012-intent-cli.md` (recommend `braid replace`
   on a 2-disk pool with one missing device) and must surface
   before the membership lookup -- otherwise a 2-disk RAID1 pool
   with one missing device and unenriched membership would report
   `NoMemberForDevid` and hide the actionable repair path.
3. BEFORE the expensive `check_relocation_space` probe, so the
   structural "your pool.json has no member for this devid"
   diagnostic beats a resource diagnostic the operator can't act
   on without first fixing membership.

Notes accumulated so far ride through on `PlanFailure::with_notes`,
matching the rest of the planner.

Drop `pre_membership` after this block; `execute()` reloads it (see
edit 3) for the same reason `RemovePlan::execute` reloads at
`cli/src/remove.rs:328` -- post-planning drift defense plus a fresh
view to mutate for the journal write.

Update the `RemoveMissingWorkPlan` construction at
`cli/src/remove_missing.rs:451-457` to populate `target_uuid` and
`target_name`.

### 3. Strip the in-execute resolution; keep one reload for the journal

In `RemoveMissingPlan::execute` (`cli/src/remove_missing.rs:155-309`):

- Delete the resolution block at `cli/src/remove_missing.rs:172-181`
  (`pre_membership` load, `resolve_removal_target` call, `resolved_devid`
  rebind).
- Read `target_uuid`, `target_name`, and `resolved_devid` from
  `work_plan` instead.
- Keep a single `load_membership` immediately before the journal-build
  block at `cli/src/remove_missing.rs:220-231`. Add a drift-defense
  recheck that uses the SAME `devid -> UUID` resolution authority as
  planning, not just a UUID-existence check:

  ```rust
  let pre_membership = membership::load_membership(params.paths)
      .map_err(|e| RemoveMissingError::Validation(format!(
          "failed to load pool membership: {e}"
      )))?;
  // Re-resolve devid -> UUID against the freshly loaded membership
  // and require the result to match what planning resolved. This
  // catches BOTH "UUID disappeared" (resolve returns NoMemberForDevid
  // via `?`) AND "devid rebound to a different UUID between planning
  // and execution" (fresh_uuid != work_plan.target_uuid). The second
  // case is the one a plain `by_uuid(&target_uuid).is_none()` check
  // misses: target_uuid could still exist as a member while devid
  // {missing_id} has migrated to a different member.
  let (fresh_uuid, _fresh_name) =
      resolve_removal_target(work_plan.missing_id, &pre_membership)?;
  if fresh_uuid != work_plan.target_uuid {
      return Err(RemoveMissingError::Validation(format!(
          "membership drift between planning and execution: devid {} \
           now resolves to a different member -- aborting to avoid \
           removing the wrong entry",
          work_plan.missing_id,
      )));
  }
  ```

Why the re-resolution shape, not `by_uuid().is_none()`: in
`remove-missing`, the persisted `devid` IS the authorized identity
binding (btrfs only reports a devid for a missing device, so devid
is the only thing the operator can pass with `--missing-id`). If
`pool.json` is mutated between planning and execution such that
devid `missing_id` now resolves to a different UUID, a UUID-existence
check would pass while `target_membership.remove_by_uuid(&target_uuid)`
would remove a member that no longer owns the btrfs devid being
removed. The journal would record the wrong member; `save_membership`
would persist that drift.

The drift recheck is the execution-time validation slot doc 022
sanctions: dry-run cannot defend against state changes between
planning and execution, but real-run can. Cost is one `pool.json` read
of a tiny file.

### 4. Update the test-only constructor and existing call sites

`remove_missing_work_plan_for_test` at
`cli/src/remove_missing.rs:556-569` becomes:

```rust
#[cfg(test)]
fn remove_missing_work_plan_for_test(
    missing_id: u64,
    will_clear_last_missing: bool,
    remaining_present: usize,
    mount_point: &MountPoint,
) -> RemoveMissingWorkPlan {
    RemoveMissingWorkPlan {
        missing_id,
        target_uuid: crate::types::LuksUuid::parse(
            "00000000-0000-0000-0000-000000000001",
        )
        .unwrap(),
        target_name: crate::types::DiskName::parse("disk-test").unwrap(),
        will_clear_last_missing,
        remaining_present,
        missing_count: if will_clear_last_missing { 1 } else { 2 },
        mount_point: mount_point.clone(),
    }
}
```

Existing callers:

- `cli/src/remove_missing.rs:1616` --
  `dry_run_render_targeted_removal_with_balance` uses the helper
  already; no change beyond the helper update.
- `cli/src/remove_missing.rs:1855` --
  `plan_preview_renders_warn_above_steps` currently constructs
  `RemoveMissingWorkPlan` as a direct struct literal (no `target_uuid`
  / `target_name` set). Adding the two new fields to the struct will
  break compilation here, NOT just at the helper. Fix by routing this
  test through `remove_missing_work_plan_for_test`:

  ```rust
  let work_plan = remove_missing_work_plan_for_test(
      3,
      true,
      2,
      &MountPoint("/mnt/storage".into()),
  );
  ```

  The helper's hard-coded `missing_count = 1` (when
  `will_clear_last_missing = true`) matches the literal's explicit
  `missing_count: 1`, so the test's semantic shape is unchanged.

`render_steps()` only references `missing_id` and `mount_point`
(`cli/src/remove_missing.rs:111-136`), so the throwaway
`target_uuid` / `target_name` defaults are behaviourally invisible
to every step-rendering test.

## New regression test

Add a sibling to the existing
`cmd_remove_missing_never_enriched_refusal_returns_structured_error`
test (`cli/src/remove_missing.rs:2306-2374`). Same fixture, swap
`.dry_run(true)`:

```rust
// Intent: when membership has no member with the requested devid,
//   `cmd_remove_missing --dry-run` must surface
//   RemoveMissingError::NoMemberForDevid (the pinned never-enriched
//   refusal) -- not silently print a "successful" preview.
//
// Why: pins the doc 022 dry-run contract for the exact case the
//   UUID-identity migration introduced. A regression that resolved
//   identity only in `execute()` would print a green plan and exit 0
//   on inputs a real run refuses -- the precise bug this fix targets.
//
// Scenario: 3-disk pool with missing devid 3; membership has every
//   member but with `devid: None`. Dry-run must refuse with the
//   pinned wording and emit zero mutating requests.
#[test]
fn cmd_remove_missing_never_enriched_refusal_in_dry_run() {
    // (same fixture setup as the existing test at line 2306)
    // ...
    let err = cmd_remove_missing(
        &runner,
        &MockFs::storage(vec![]),
        &f.remove_missing_params().missing_id(3).dry_run(true).build(),
    )
    .unwrap_err();
    match &err {
        RemoveMissingError::NoMemberForDevid { devid } => {
            assert_eq!(*devid, 3)
        }
        other => panic!("expected NoMemberForDevid, got: {other:?}"),
    }
    assert!(
        err.to_string().contains("no member in membership has devid 3"),
        "expected pinned NoMemberForDevid wording; got: {err}"
    );
    // Membership untouched, no journal, no inhibitor, no mutating
    // CmdRequests (mirrors the real-run test's invariants).
    assert_eq!(pre_bytes, post_bytes, "...");
    assert_eq!(f.inhibitor.acquire_count(), 0, "...");
    assert!(journal::load_journal(&f.paths).unwrap().is_none(), "...");
    assert!(
        runner.requests().iter().all(|c| !matches!(
            c,
            CmdRequest::BtrfsDeviceRemove { .. }
                | CmdRequest::BtrfsBalanceRaid1Soft { .. }
                | CmdRequest::CryptsetupClose { .. }
                | CmdRequest::BtrfsDeviceScanForget { .. }
        )),
        "..."
    );
}
```

Pin the same invariants as the real-run twin so a future regression
that drops the planner-side check is caught regardless of mode.

Add a second test that pins the execute-time drift recheck: build a
valid plan, mutate `pool.json` between planning and execution to
rebind devid 3 to a different UUID, then call `plan.execute(...)`
and assert the drift error fires before any mutating CmdRequest.

```rust
// Intent: when pool.json is mutated between `plan_remove_missing` and
//   `RemoveMissingPlan::execute` such that devid {missing_id} now
//   resolves to a different UUID, execute() must abort BEFORE issuing
//   any btrfs mutation, journal write, or membership save.
//
// Why: the persisted devid is the authorized identity binding for
//   missing devices. A UUID-existence-only recheck would let a stale
//   plan remove the wrong member from pool.json (journal records
//   target_uuid; btrfs removes the devid currently bound to a
//   different uuid). This test pins the re-resolution + equality
//   check against the planned UUID.
//
// Scenario: 3-disk pool with missing devid 3 bound to disk3's UUID.
//   Plan resolves to disk3. Before execute runs, rewrite pool.json
//   so devid 3 is now bound to a different (disk2's) UUID. Execute
//   must error with the drift Validation message and emit zero
//   mutating CmdRequests.
#[test]
fn execute_aborts_when_devid_rebinds_between_plan_and_execute() {
    // (3-disk fixture; devid 3 bound to disk3's UUID in pool.json)
    let f = PoolFixture::three_disk_devids_pinned();
    let (runner, _remove_done) =
        RemoveMissingPool::three_disk_one_missing().install(MockRunner::default());
    let plan = plan_remove_missing(
        &runner,
        &MockFs::storage(vec![]),
        &f.remove_missing_params().missing_id(3).build(),
    )
    .expect("planning should succeed for valid devid 3 -> disk3");

    // Mutate pool.json: rebind devid 3 to disk2's UUID, leaving disk3
    // member intact but devid-less.
    let mut m = membership::load_membership(&f.paths).unwrap();
    if let Some(disk2) = m.by_name_mut(&DiskName::parse("disk2").unwrap()) {
        disk2.devid = Some(3);
    }
    if let Some(disk3) = m.by_name_mut(&DiskName::parse("disk3").unwrap()) {
        disk3.devid = None;
    }
    membership::save_membership(&m, &f.paths).unwrap();

    // Capture the drifted bytes BEFORE execute so we can assert
    // pool.json is untouched after the recheck fires.
    let drifted_bytes = std::fs::read(f.paths.pool_json()).unwrap();

    let err = plan
        .execute(
            &runner,
            &MockFs::storage(vec![]),
            &f.remove_missing_params().missing_id(3).build(),
        )
        .unwrap_err();
    match &err {
        RemoveMissingError::Validation(msg) => assert!(
            msg.contains("membership drift"),
            "expected drift wording; got: {msg}"
        ),
        other => panic!("expected Validation(drift), got: {other:?}"),
    }
    // Zero mutating CmdRequests: drift recheck fires before
    // pool_remove_device_using.
    assert!(
        runner.requests().iter().all(|c| !matches!(
            c,
            CmdRequest::BtrfsDeviceRemove { .. }
                | CmdRequest::BtrfsBalanceRaid1Soft { .. }
        )),
        "drift recheck must fire before any btrfs mutation"
    );
    // Zero recovery state stranded: drift recheck must fire BEFORE
    // journal::write_journal and BEFORE membership::save_membership.
    // Without these assertions, a regression that wrote pending-op.json
    // before the recheck would still pass the CmdRequest check above.
    assert!(
        journal::load_journal(&f.paths).unwrap().is_none(),
        "drift recheck must fire before pending-op.json is written"
    );
    let post_bytes = std::fs::read(f.paths.pool_json()).unwrap();
    assert_eq!(
        drifted_bytes, post_bytes,
        "drift recheck must fire before save_membership; pool.json \
         must remain byte-for-byte the drifted state"
    );
}
```

The exact mutation API (`by_name_mut`, or building a fresh
`PoolMembership` and saving) is an implementation detail of the
membership module that the implementer should adapt to whatever is
available; what the test pins is the post-mutation state and the
execute-time error.

## Files to modify

- `cli/src/remove_missing.rs` -- the only production file touched.
  Specifically:
    - `RemoveMissingWorkPlan` struct (lines 97-104) -- add two fields
      (`target_uuid`, `target_name`).
    - `plan_remove_missing` (lines 319-459) -- insert resolution
      AFTER the 2-disk RAID1 reject at line 428 and BEFORE
      `check_relocation_space` at line 437; thread fields into work
      plan at line 451.
    - `RemoveMissingPlan::execute` (lines 155-309) -- drop in-execute
      resolution at lines 172-181, add re-resolution drift recheck
      (fresh UUID must equal `work_plan.target_uuid`) before journal
      build, read identity from `work_plan`.
    - `remove_missing_work_plan_for_test` (lines 556-569) -- populate
      the two new fields with throwaway defaults.
    - `plan_preview_renders_warn_above_steps` test
      (`cli/src/remove_missing.rs:1855`) -- the direct struct literal
      becomes a call to `remove_missing_work_plan_for_test`.
    - Tests module -- add
      `cmd_remove_missing_never_enriched_refusal_in_dry_run` and
      `execute_aborts_when_devid_rebinds_between_plan_and_execute`.

No other files. `resolve_removal_target` is private to
`cli/src/remove_missing.rs` and called only from `execute()` today
(verified -- grep finds the definition at line 58, callers at line 180
and a unit test at line 1598).

## Functions / types reused, not invented

- `membership::load_membership` (`cli/src/membership.rs`) -- already
  the canonical reader.
- `PoolMembership::by_devid` and `by_uuid`
  (`cli/src/membership.rs`) -- already the resolvers.
- `resolve_removal_target` (`cli/src/remove_missing.rs:58-66`) --
  kept as-is; call site moves from `execute` to `plan_remove_missing`.
- `PlanFailure::with_notes` (`cli/src/preview.rs`) -- the existing
  preserved-notes-on-error shape, used by every other failure branch
  in this planner.

## Verification

End-to-end check that the fix lands without regressions:

1. **Rust unit tests:** `just test-rust` --
    - new `cmd_remove_missing_never_enriched_refusal_in_dry_run` test
      passes (planner-side resolution surfaces `NoMemberForDevid` in
      dry-run).
    - new `execute_aborts_when_devid_rebinds_between_plan_and_execute`
      test passes (execute-time drift recheck fires before any
      mutation when `pool.json` is rewritten between plan and
      execute).
    - existing `cmd_remove_missing_never_enriched_refusal_returns_structured_error`
      still passes (the planner-side check now fires earlier, but the
      assertions -- no inhibitor, no journal, no mutating requests,
      pinned error wording -- still hold).
    - existing dry-run preview tests
      (`cli/src/remove_missing.rs:1741`, `:1816`, `:1936`, `:1990`,
      `:2042`, `:2086`) still pass; they call `plan_remove_missing`
      with enriched membership where resolution succeeds, so the new
      code path is transparent to them.
    - `plan_preview_renders_warn_above_steps` at line 1853 still
      passes after the struct literal is routed through
      `remove_missing_work_plan_for_test`.
    - `dry_run_render_targeted_removal_with_balance` at line 1616 still
      passes; `render_steps()` only reads `missing_id` /
      `mount_point`.
2. **VM tests:** `just test-vm` -- exercise the live remove-missing
    path end-to-end. The change is internal to `plan` vs `execute`
    routing of an already-pinned check; no new VM scenario is
    required, but the existing VM coverage should regression-test the
    real-run path.
3. **Manual smoke check (optional):** From inside a 3-disk VM with one
    member's devid scrubbed from `pool.json`, run
    `braid remove-missing --dry-run --missing-id <devid>`. Pre-fix:
    exits 0 with a "successful" plan. Post-fix: exits non-zero with
    `no member in membership has devid <devid>`.

## Out of scope

- Other `execute()`-only failure modes in `RemoveMissingPlan::execute`
  (journal writes, `pool_remove_device_using`, `save_membership`,
  `maybe_restore_raid1`) are genuine mutations that dry-run
  intentionally cannot perform. Doc 022 explicitly carves them out
  ("checks that require a passphrase or a mapper that was closed
  during planning"). They are not dry-run-divergence bugs.
- No changes to `resolve_removal_target` signature, to membership
  error types, or to the pinned `NoMemberForDevid` wording.
- No sweep of other commands. `remove`, `replace`, and `recover`
  already resolve identity inside their planners and are correct
  under the doc 022 contract.
