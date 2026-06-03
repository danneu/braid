# Plan: execute-time 2->1 survivor-capacity re-check for `braid remove`

## Context

`braid remove`'s single-survivor (2->1) capacity preflight (`check_single_survivor`
via `check_eviction_space`) runs **only** in `plan_remove` (`cli/src/remove.rs#plan_remove`).
`RemovePlan::execute` re-validates topology twice (`validate_pool_topology`, pre- and
post-journal) but never re-checks capacity.

The 2->1 case is the dangerous one: the RAID1->single step ships `btrfs balance ... -f`
(`cli/src/remove.rs#RemoveWorkPlan::render_steps`), which skips btrfs-progs' missing-device
safety timeout. Between plan and execute the operator can stall at the `yes` prompt and the
inhibitor-acquire wait while the pool keeps taking writes (backup job, SMB client, cron `dd`).
A survivor that had room at plan time can be over-committed by execute time; the `-f` balance
then crashes the fs read-only mid-migration with `pending-op.json` already on disk -- exactly
the failure mode `tests/repro/remove-2to1-undersized-survivor.py` exists to prevent, but that
repro only covers the **plan** boundary (it fills the pool with `--yes` *before* running remove).

ADR 022 (`docs/design/decisions/022-dry-run-preview-model.md`) explicitly licenses
"execution-time validation that dry-run intentionally cannot do." Capacity is volatile state
that can drift across the plan/execute gap, so it belongs there too -- for the fail-closed 2->1
branch.

### Pivot vs. the original finding

The finding proposed re-running the check **post-journal** with the "run `braid recover`"
remediation. That is the wrong slot. An over-committed survivor detected before any mutation is
a "command never started" condition -- principle 3
(`docs/design/principles.md#3-safe-by-construction-operations`) requires such failures to occur
**above** `journal::write_journal` so they do not strand `pending-op.json`. The repro's Phase 4
("No pending-op.json after preflight refusal") encodes exactly this invariant. The post-journal
slot is reserved for the MISSING-device gate, which is *inherently* a recovery situation (a disk
vanished); capacity over-commit is cleanly abortable and must stay clean.

The binding precedent for the exact placement is remove's **own** pre-journal
`validate_pool_topology` gate: it sits post-confirm, post-inhibitor, pre-journal, and its drift
test (`execute_rejects_when_pool_json_drifts_after_planning`) asserts `acquire_count == 1` and no
journal -- the same position and assertions this re-check adopts. `replace.rs` is the precedent for
the broader *shape* only -- re-check live pool state in `execute` before the journal write
(`cli/src/replace.rs#verify_replace_execute_live_pool_uuid` + `execute_rechecks_live_pool_*` tests).
Note that replace places its re-check *before* the inhibitor, so its rejection tests assert
`acquire_count == 0`; remove deliberately differs by matching its own topology gate's post-inhibitor
placement (`acquire_count == 1`).

**Intended outcome:** capacity is re-validated at the latest clean point in `execute`
(post-confirm/inhibitor, pre-journal), closing the operator-stall drift window while still
failing clean -- no stranded journal, no forced recovery.

## Change 1 -- refactor `check_single_survivor` to take a devid

`cli/src/remove.rs#check_single_survivor` currently takes `target: &PoolDevice` but reads only
`target.devid` (survivor lookup + error message). Change the parameter to `target_devid: u64` so
both planning and execute can call it -- execute has `work_plan.target_devid`, not a live
`PoolDevice`.

- Signature: `target: &PoolDevice` -> `target_devid: u64`.
- Body: `.find(|d| d.devid != target_devid)` and the error-message interpolation use `target_devid`.
- Update the one plan-time call site inside `check_eviction_space` (`cli/src/remove.rs#check_eviction_space`,
  the `remaining == 1` branch): pass `target.devid` instead of `target`.
- Update `check_single_survivor`'s doc comment. It currently reads "2->1 branch of
  `check_eviction_space`", which will misstate ownership once the second caller lands. Reframe it as
  the shared single-survivor capacity helper invoked at **both** the planning preflight
  (`check_eviction_space`) and the pre-journal execute gate (`RemovePlan::execute`), per AGENTS.md's
  "capture call-site coupling" doc-comment rule. Keep the existing fail-closed-rationale pointer to
  the `check_eviction_space` docstring.

`check_eviction_space` itself is unchanged in signature (still `target: &PoolDevice`; its `>=2`
branch keeps using `target.devid`). Only the inner helper's signature changes.

## Change 2 -- add the pre-journal re-check in `RemovePlan::execute`

In `cli/src/remove.rs#RemovePlan::execute`, immediately **after** the pre-journal
`validate_pool_topology` block and **before** `membership::load_membership` /
`journal::write_journal`, add:

```rust
// (Pre-journal) survivor-capacity re-check for the fail-closed 2->1 branch.
// Capacity validated at plan time can go stale across the confirmation prompt +
// inhibitor-acquire window while the pool keeps taking writes. Re-running it here
// -- above journal::write_journal -- catches an over-committed survivor before the
// irreversible `-f` balance and fails CLEAN (no stranded pending-op.json), because
// no mutation has happened yet (principle 3,
// docs/design/principles.md#3-safe-by-construction-operations). The >=2-survivor
// branch is intentionally NOT re-checked: `btrfs device remove` ENOSPCs cleanly
// there (see check_eviction_space docstring).
if work_plan.remaining == 1 {
    check_single_survivor(runner, &work_plan.mount_point, work_plan.target_devid)?;
}
```

Notes:
- `check_single_survivor` already returns `RemoveError::Validation` carrying clean-abort wording
  ("not enough space on surviving device ... Free up space ... or `braid add` a larger disk",
  from `cli/src/preflight.rs#check_single_survivor_capacity`). The `?` propagates it. No new
  error message or `recover` remediation is introduced.
- It is fail-closed on every uncertainty (spawn/parse error, survivor absent) -- at the
  pre-journal position those uncertainties also abort clean, consistent with the existing
  pre-journal `validate_pool_topology`.
- Gating on `work_plan.remaining == 1` (the planned remaining) is safe: the immediately-preceding
  `validate_pool_topology` rejects any device-set drift, so if topology matches the plan, the
  survivor count is still 1.
- Placement is post-inhibitor (like the existing pre-journal topology check), so a clean abort
  here acquires-then-releases the inhibitor via its RAII guard -- identical to today's
  topology-drift behavior; the new test asserts `acquire_count == 1`.

## Change 3 -- docs

Update `docs/design/decisions/012-intent-cli.md` (status: Active), "ENOSPC pre-flight check"
section. Add one sentence to the `remove (2→1)` bullet (or the fail-closed paragraph) stating the
single-survivor capacity check runs at plan time **and** is re-run as a pre-journal gate in
`execute`, closing the plan/execute drift window and failing clean (no `pending-op.json`).

Match the file's existing notation: 012 uses the Unicode arrow `2→1` throughout, so write `2→1`
(not ASCII `2->1`) in the edit -- the global ASCII-preference rule excepts files already in the
Unicode form. The code comment in Change 2 carries the principle-3 rationale; no edit to
`principles.md` is needed (this is an instance of principle 3, not a new principle).

## Tests

### New unit test (`cli/src/remove.rs` `mod tests`)

`execute_rechecks_survivor_capacity_before_journal` -- mirrors
`replace.rs#execute_rechecks_live_pool_*` and the assertion shape of
`cli/src/remove.rs` `execute_rejects_when_pool_json_drifts_after_planning`. Model "capacity was
fine at plan, degraded by execute" by planning against a healthy runner and executing against an
over-committed one (no on-disk mutation needed; the runner arg differs between `plan_remove` and
`plan.execute`):

```
let f = PoolFixture::two_disk_healthy();
let healthy = RemovalPool::two_disk().install(MockRunner::default());
let plan = plan_remove(&healthy, &fs, &params).expect("plan succeeds with healthy survivor");

// over-committed runner: healthy probe topology (so validate_pool_topology passes),
// but BtrfsDeviceUsageRaw + BtrfsFilesystemDfJson report survivor (devid 1) over capacity.
let overcommitted = RemovalPool::two_disk().install(MockRunner::default())
    .with_handler(|req| match req {
        CmdRequest::BtrfsDeviceUsageRaw { .. } => Some(Ok(overcommitted_survivor_usage())),
        CmdRequest::BtrfsFilesystemDfJson { .. } => Some(Ok(overcommitted_survivor_df())),
        _ => None,
    });

let result = plan.execute(&overcommitted, &fs, &params);
// assert Err(RemoveError::Validation(msg)) with msg.contains("not enough space on surviving device")
// assert !f.paths.pending_op_json().exists()   // pre-journal: clean abort
// assert f.inhibitor.acquire_count() == 1       // post-inhibitor placement
```

Include the standard three-section test preamble (Intent / Why it exists / Scenario) per
`docs/dev/testing.md`.

### Fixtures (`cli/src/test_fixtures/remove.rs`)

No existing fixture produces an over-committed survivor. Add helpers built on the existing
`DeviceUsageSpec::live` + `device_usage_raw_body` (and a small df JSON literal): survivor is the
**non-target** devid (devid 1 when removing disk2/devid 2) with a small `device_size`, and df
reports `data + 2*metadata + 2*system > device_size - device_slack` (e.g. device_size 100 MiB;
df Data.used 60 MiB + Metadata.used 30 MiB -> needed 120 MiB > 100 MiB usable). Any new
`pub(crate)` fixture fn gets a `///` doc comment per AGENTS.md.

### Existing tests stay green

`two_to_one_remove_invokes_survivor_capacity_preflight` already drives a healthy 2->1 remove
through `cmd_remove`; with this change its runner reports a healthy survivor at execute too, so
the new check passes and the happy path is covered. Its `position()`-based "usage/df precede
balance" assertions still hold (first occurrence is the plan-time call). The plan-time
fail-closed tests (`check_eviction_space_2to1_*`) are unaffected by the inner-helper signature
change (they call `check_eviction_space`, not `check_single_survivor`).

## Verification

- `just test-rust` -- runs the new test plus the existing `remove`/`preflight` unit suites.
- `just test-vm remove-2to1-undersized-survivor` -- the plan-time repro is unchanged and must
  still pass (plan path untouched).
- No parser/fixture-refresh obligation: no parser code or tool-output shape changes, so the
  golden-fixture and `just test-parsers` lanes are not implicated.

A VM repro of the *execute-time* TOCTOU is intentionally not added: `braid remove` is a single
in-process operation, so writes cannot be deterministically injected during the in-process
confirm/inhibitor window. The unit test at the `execute` seam is the faithful,
structure-insensitive guard -- the same level at which `execute_rejects_when_pool_json_drifts_after_planning`
covers the analogous topology-drift-at-execute case.

## Non-goals

- **Recover:** confirmed out of scope. `cli/src/recover.rs` never re-issues
  `pool_balance_single`; `recover_skips_paused_balance_resume_for_remove` documents that recover
  rebuilds membership to the pre-state, skips a paused 2->1 balance, and directs the operator to
  re-run `braid remove` (which re-enters `plan_remove` + the new execute check).
- **`>=2`-survivor execute re-check:** unnecessary -- `btrfs device remove` ENOSPCs cleanly with
  two or more survivors (existing warn-and-proceed policy, documented on `check_eviction_space`).
- **Quiescing writes during the balance** (read-only remount for the 2->1 conversion): the only
  thing that closes the *during-balance* write race (which no preflight can bound). A larger
  design change with client-facing impact; flagged for a separate initiative, not this fix.
