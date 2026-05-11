# Plan: guard `braid remove` against pool.json drift before journaling

## Context

`RemovePlan::execute` at `cli/src/remove.rs:288-300` loads `pre_membership`
from pool.json, clones it into `target_membership`, then calls
`target_membership.disks.remove(&work_plan.name)` without checking the
return value. If pool.json has drifted -- i.e. live btrfs has the target
mapper (so the planner accepted it at `remove.rs:422`) but pool.json
never recorded a corresponding entry -- the `disks.remove` call is a
silent no-op. The journal then records identical `pre_membership` and
`target_membership` under `OpKind::Remove { name }`, and the
irreversible btrfs device-remove proceeds.

This is a recovery-safety hazard, not a future-only audit concern. If
the remove is interrupted (crash, power loss, kernel hang) after the
journal write but before completion, `braid recover` runs on the next
boot. For `OpKind::Remove`, recovery walks the journal:

- `union_memberships(&journal)` at `recover.rs:1118` -- pre and target
  agree, so the union also omits the drifted disk.
- `mount_membership_for_recover` at `recover.rs:3268` returns that
  union for `OpKind::Remove`.
- `mount::plan_open_pool` opens LUKS only for union members, so the
  drifted disk's mapper is never opened. btrfs assembles degraded or
  refuses to mount.
- `build_membership_from_live_pool` (`recover.rs:1737`) rebuilds
  pool.json from whatever live btrfs reports under that incomplete
  mount, then `recovery_guidance` reports recovery as successful.

End-state: pool.json continues to omit the disk, the user has a
stranded LUKS member with no signal that anything is wrong, and the
journal artifact that triggered recovery had identical pre/target
snapshots. The cure -- once it happens -- is silent.

The sibling commands already guard this transition during planning,
above the dry-run gate, per `docs/decisions/022-dry-run-preview-model.md:30-34`
(`plan_*()` owns config/state loading and preflight; `execute()` may
only add validation that needs passphrases or post-planning mapper
state):

- `replace.rs:934-964` in `plan_replace` calls
  `build_replacement_membership`, which errors at `replace.rs:1519-1524`
  with `"'old_name' not found in pool.json membership -- no disk entry
  has this name. Pool membership may need manual repair."` This is the
  exact bug we are fixing, already fixed there -- see the test
  preamble at `replace.rs:2925-2939` calling out the same
  `HashMap::remove` silent no-op.
- `remove_missing.rs:170-174` in `execute` calls
  `resolve_removal_target`, which errors at `remove_missing.rs:44-50`
  with `"devid {devid} not found in pool.json membership -- no disk
  entry has this device ID. Pool membership may need manual repair."`

`remove.rs` is the only one of the three callers without this pin
between pool.devices (live btrfs) and pool.json (membership), and
unlike `replace.rs`, the check is not even planner-visible -- dry-run
would print a successful plan today even when the same drift would
cause the real run to write a misleading journal.

## Change

Add the membership-presence check in **two places**:

1. **Primary check in `plan_remove`**, after the live target is found
   at `remove.rs:422-440` and after `check_no_missing_devices`. This is
   the canonical placement per `022-dry-run-preview-model.md:30-34` and
   the established `replace.rs:934-964` pattern. It makes `--dry-run`
   reject pool.json drift, surfaces the failure with accumulated
   preview notes, and aborts before any inhibitor or journal work.
2. **Defense-in-depth check in `RemovePlan::execute`**, immediately
   after the existing `membership::load_membership` call at
   `remove.rs:288-289` and before `journal::build_journal`. This closes
   the TOCTOU window between `plan_remove` and `execute`: between dry-run
   gate, interactive confirmation, and inhibitor acquisition, a concurrent
   operator could edit pool.json. Mirrors the in-execute placement that
   `remove_missing.rs:170-174` already uses.

Both checks share one error wording, modeled on `replace.rs:1519-1524`
so all three callers surface a uniform message for the same class of
drift.

### File to modify

- `cli/src/remove.rs` -- new membership check inside `plan_remove` after
  target resolution, plus a defense-in-depth check inside
  `RemovePlan::execute`.

### Exact shape (plan_remove)

Insert between the existing `check_no_missing_devices` block at
`remove.rs:442-449` and the `RemoveWorkPlan::new` call at `remove.rs:455`:

```rust
// Validate pool.json membership lists the target. The planner already
// confirmed live btrfs owns it (line 422); without this pin, drift where
// pool.json omits a present disk would let target_membership.disks.remove
// silently no-op inside execute, write a misleading journal, and on a
// later interrupted-remove recovery leave a stranded LUKS member. See
// docs/decisions/022-dry-run-preview-model.md for why this belongs in
// plan_remove, not execute.
let pre_membership = match membership::load_membership(params.paths) {
    Ok(m) => m,
    Err(e) => {
        return RemovePlanReport {
            notes: std::mem::take(&mut notes),
            result: Err(RemoveError::Validation(format!(
                "failed to load pool membership: {e}"
            ))),
        };
    }
};
if !pre_membership.disks.contains_key(params.name) {
    return RemovePlanReport {
        notes: std::mem::take(&mut notes),
        result: Err(RemoveError::Validation(format!(
            "'{}' not found in pool.json membership -- no disk entry has this name. \
             Pool membership may need manual repair.",
            params.name
        ))),
    };
}
```

The loaded `pre_membership` is intentionally discarded here -- `execute`
reloads it for the defense-in-depth check below. Caching it on the work
plan would duplicate state across the plan/execute boundary that
`022-dry-run-preview-model.md` keeps clean, and the second read is
cheap.

### Exact shape (execute)

Replace this block at `remove.rs:288-291`:

```rust
let pre_membership = membership::load_membership(params.paths)
    .map_err(|e| RemoveError::Validation(format!("failed to load pool membership: {e}")))?;
let mut target_membership = pre_membership.clone();
target_membership.disks.remove(&work_plan.name);
```

with:

```rust
let pre_membership = membership::load_membership(params.paths)
    .map_err(|e| RemoveError::Validation(format!("failed to load pool membership: {e}")))?;
if !pre_membership.disks.contains_key(&work_plan.name) {
    return Err(RemoveError::Validation(format!(
        "'{}' not found in pool.json membership -- no disk entry has this name. \
         Pool membership may need manual repair.",
        work_plan.name
    )));
}
let mut target_membership = pre_membership.clone();
target_membership.disks.remove(&work_plan.name);
```

### What does NOT change

- The journal format, `OpKind::Remove`, and the `recover` flow stay
  untouched. The bug is upstream of journal write; once the planner
  rejects the drift, no journal exists to interpret.
- `RemoveWorkPlan` does not gain new state. Both checks read pool.json
  directly at their respective points.
- The pre- and post-journal `validate_pool_topology` calls at
  `remove.rs:266-285` and `remove.rs:310-331` are untouched -- they
  validate live btrfs topology, not pool.json membership.

## Test

Add two unit tests in the inline `mod tests` block of
`cli/src/remove.rs`, near the other execute-path validation tests
(neighborhood of `pre_journal_same_mapper_replacement_rejected` at
~line 755 and `cmd_remove_prunes_acked_stats_for_removed_devid` at
~line 847). Both tests model `cmd_replace_missing_path_rejects_old_name_absent_from_membership`
at `replace.rs:2925-2966`.

### Test 1 (dry-run regression)

`cmd_remove_dry_run_rejects_when_target_absent_from_pool_json`

```text
// Intent: plan_remove must reject pool.json drift before the dry-run
// gate, so --dry-run never prints a successful plan that the real run
// would later refuse.
//
// Why it exists: 022-dry-run-preview-model.md puts state loading and
// preflight in plan_*(). Without a planner-visible membership check,
// dry-run drifts from real-run on the same input -- the exact failure
// 022 was written to prevent.
//
// Scenario: live btrfs reports disk1+disk2+disk3 (so plan_remove
// accepts disk1 as the target), but pool.json only contains disk2+disk3.
// The operator runs `braid remove --dry-run disk1`. The command must
// fail with a Validation error citing pool.json, with no inhibitor
// acquired and no journal written.
```

Mechanics:
1. Build `PoolFixture::three_disk_healthy()` (defined at
   `cli/src/test_fixtures/remove.rs:75`).
2. Immediately overwrite pool.json by constructing a `PoolMembership`
   containing only disk2 and disk3 and calling
   `membership::save_membership(&drifted, &paths)`.
3. Install the matching `RemovalPool` mock runner so live btrfs still
   reports all three disks.
4. Call `cmd_remove` with `name = "disk1"`, `dry_run = true`, `yes = true`.
5. Assert:
   - `matches!(result, Err(RemoveError::Validation(_)))` with message
     containing `"not found in pool.json membership"` and `"disk1"`.
   - `paths.pending_op_json()` does not exist.
   - Inhibitor `acquire_count() == 0`.
   - pool.json on disk is unchanged (still missing disk1).

### Test 2 (execute-side TOCTOU guard)

`execute_rejects_when_pool_json_drifts_after_planning`

This test must NOT pre-seed the drift, because then `plan_remove` would
short-circuit and the execute-side check would never run. It must
construct the planner-pass-then-drift sequence that the defense-in-depth
check exists to handle, so the test fails if that check is omitted.

```text
// Intent: RemovePlan::execute must reject pool.json drift introduced
// between plan_remove and execute, before journal::build_journal,
// even after the inhibitor has been acquired.
//
// Why it exists: between dry-run gate, interactive confirmation, and
// inhibitor acquisition there is a window where a concurrent operator
// (or another braid process) could rewrite pool.json. Without the
// execute-side check, target_membership.disks.remove silently no-ops
// in that window and a misleading journal lands on disk. If the
// remove is later interrupted, recovery walks the journal union
// (recover.rs:1118 / 3268), opens only union members, rebuilds
// pool.json from an incomplete live view, and silently strands the
// drifted disk's LUKS.
//
// Scenario: pool.json initially records disk1+disk2+disk3. The
// operator runs `braid remove disk1`. plan_remove succeeds. Before
// execute reaches the membership check (e.g. concurrent admin edit
// during the confirmation pause), pool.json is rewritten to contain
// only disk2+disk3. execute must reject with a Validation error
// citing pool.json, and pending-op.json must not be written.
```

Mechanics:
1. Build `PoolFixture::three_disk_healthy()`. pool.json now has
   disk1+disk2+disk3 and live btrfs (via the mock runner) reports the
   same three disks.
2. Install the matching `RemovalPool` mock runner.
3. Build `RemoveParams` with `name = "disk1"`, `yes = true`,
   `dry_run = false`.
4. Call `plan_remove(&runner, &fs, &params)` directly. Assert the
   report's `result` is `Ok(plan)`; extract `plan`.
5. Overwrite pool.json: construct a `PoolMembership` containing only
   disk2 and disk3 and call `membership::save_membership(&drifted,
   &paths)`. This simulates the TOCTOU edit.
6. Call `plan.execute(&runner, &fs, &params)`.
7. Assert:
   - `matches!(result, Err(RemoveError::Validation(msg)))` where `msg`
     contains `"not found in pool.json membership"` and `"disk1"`.
   - `paths.pending_op_json()` does not exist (the check rejected
     before `journal::write_journal` at `remove.rs:299`).
   - Inhibitor `acquire_count() == 1` -- not 0. The execute-side check
     runs *after* `sleep_inhibitor.acquire` at `remove.rs:253-257`, so
     the inhibitor is held by the time the check fires. Asserting `== 1`
     pins the path: the test reached execute, acquired the inhibitor,
     loaded pool.json, ran the check, and rejected. Asserting `== 0`
     would be wrong and would mask a regression that moves the check
     above the inhibitor (which `022-dry-run-preview-model.md` does not
     require and which would change observable startup ordering).
   - pool.json on disk is the drifted state from step 5 (no `save_membership`
     happened after rejection).
8. **Negative check that proves this test exercises the execute-side
   guard:** the test exists specifically to fail if the execute-side
   defense-in-depth check is omitted. Reviewers should confirm that
   removing only the `execute()` check (keeping the `plan_remove`
   check) causes this test to fail. Test 1 alone cannot do this --
   with Test 1's pre-seeded drift, `plan_remove` rejects first and the
   execute path is never entered.

### What these tests pin

Test 1 is structure-insensitive: it observes only the dry-run-boundary
behavior (error type, wording, no journal, no inhibitor, unchanged
pool.json). Any planner-side reorganization that still rejects pool.json
drift before the dry-run gate keeps it green.

Test 2 is intentionally structure-sensitive in one specific way: it
pins that an execute-side guard exists between `load_membership` and
`journal::build_journal`. Collapsing the two checks into a single
planner-only check would break Test 2 -- and that is the point.
Beyond that pin it observes only behavior (error type, wording, no
journal, inhibitor `== 1`, unchanged pool.json), so refactors that
keep an execute-side check (rename it, move it into a helper, fold it
into a shared validator shared with `remove_missing.rs`) stay green.

## Verification

After implementing:

1. `just test-rust` -- the two new tests plus all existing remove tests
   must pass. The fix is Rust-only; no VM tests are needed.
2. `grep -n "not found in pool.json membership" cli/src/` -- all three
   callers (`remove.rs`, `remove_missing.rs`, `replace.rs`) should
   report this same phrase, confirming wording consistency.
3. `cargo build -p braid-cli` -- compiles clean.

No NixOS VM test is needed: pool.json drift is a state-shape concern,
not a tool-output concern, and the established siblings (`replace.rs`,
`remove_missing.rs`) rely on unit tests only.
