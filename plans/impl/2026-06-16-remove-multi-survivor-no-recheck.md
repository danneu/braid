# Plan: pin the absence of an execute-time survivor-capacity re-check on the `remaining >= 2` path

## Context

`RemovePlan::execute` (`cli/src/remove.rs#execute`) re-checks survivor capacity at
execute time **only** on the fail-closed 2->1 path:

```rust
if work_plan.remaining == 1 {
    check_single_survivor(runner, &work_plan.mount_point, work_plan.target_devid)?;
}
```

The `remaining >= 2` path is *intentionally not re-checked* at execute time -- it
relies on `btrfs device remove` ENOSPCing cleanly. This asymmetry is load-bearing
and triple-documented with explicit "Do **not** unify" admonitions in the
`EvictionCheck` and `check_eviction_space`/`check_single_survivor` docstrings
(`cli/src/remove.rs:663-705`, `:771-839`) and the inline gate comment
(`cli/src/remove.rs:334-342`), and it has a dedicated fix commit behind the
positive half (`64a076c5 fix(remove): re-check survivor capacity at execute time
for 2->1 removes`).

**The gap:** only the *positive* half of the asymmetry is pinned, by
`execute_rechecks_survivor_capacity_before_journal` (`cli/src/remove.rs:1397`). The
*negative* half -- that a `remaining >= 2` execute issues **no** survivor-capacity
probe -- is unpinned. A well-intentioned "make capacity checks consistent" refactor
that adds a fail-closed re-check to the `>= 2` execute path would refuse a valid
remove on a transient probe error. That path is warn-and-proceed by policy and
leans on `btrfs device remove` to ENOSPC cleanly, so a hard re-check there is wrong
on its face. And because the existing `remaining == 1` gate sits *above*
`journal::write_journal` (`cli/src/remove.rs:343` vs `:379`), a re-check bolted onto
that gate fails clean -- but one placed *after* the journal write would additionally
strand `pending-op.json`, forcing recovery for an environmental hiccup. No test
catches either today.

This is the pivot recommended by `/verify-issue`: the finding's intent is correct,
but its proposed assertion mechanics ("no probe after the `BtrfsDeviceRemove` index
minus the journal write") are not implementable as written -- `journal::write_journal`
is a filesystem write, not a `CmdRequest`, so there is no such index in
`runner.requests()`. We replace it with the two-runner design the positive test
already establishes.

Intended outcome: a single, focused regression test that fails if any future edit
adds a survivor-capacity probe to the `remaining >= 2` execute path.

## Approach

Add one Rust unit test in the `remove.rs` test module, co-located immediately after
the positive test (`execute_rechecks_survivor_capacity_before_journal`, ends
~`cli/src/remove.rs:1450`) so the asymmetry pair reads together.

**Design (mirror the positive test's two-runner split, both runners healthy):**

- Plan a 3->2 remove (`PoolFixture::three_disk_healthy()`, default `name="disk2"`)
  on one `RemovalPool::three_disk().install(MockRunner::default())` runner.
- Execute the resulting plan on a **separate, fresh** healthy `three_disk` runner.
  The separate runner is the crux: it isolates execute-phase requests so the
  plan-time `>= 2` `BtrfsDeviceUsageRaw` probe (issued by `check_eviction_space`,
  `cli/src/remove.rs:724`) lands on the plan runner, not the one under assertion.
- Default `remove_params()` is `name="disk2", yes=true`, so `execute`'s confirm
  block (`if !params.yes`) is skipped and the test reaches the mutation path
  directly -- identical to how the positive test runs `plan.execute` without arming
  `confirm`.

**Assertions (on the execute runner's `requests()`):**

1. `plan.execute(...)` returns `Ok(())` -- the 3->2 remove completes on a healthy pool.
2. **The invariant:** no `CmdRequest::BtrfsDeviceUsageRaw` and no
   `CmdRequest::BtrfsFilesystemDfJson` appear. This is strictly stronger and simpler
   than the finding's "between journal and device remove" framing: the two-runner
   split captures the entire execute phase (pre- and post-journal), and the `>= 2`
   path probes in neither, so "no capacity probe anywhere in the execute phase"
   covers the post-journal strand concern *and* a wrongly-placed pre-journal probe.
3. `CmdRequest::BtrfsDeviceRemove` is present -- proves the `Ok` is not a vacuous
   early return; the remove genuinely ran (mirrors the positive test's
   "must abort before the balance" non-vacuity guard).
4. `!f.paths.pending_op_json().exists()` -- the journal is cleared on success,
   nothing stranded (direct tie to the finding's impact; mirrors the positive
   test's `pending-op.json` assertion, inverted: refusal => never written, success
   => cleared).

**Why this is structure-insensitive and behavioral:** it observes only the external
command stream at the `MockRunner` boundary (the same seam the positive test,
`journal_survives_evict_failure`, and `remove_two_disk_pool_balances_single_before_device_remove`
all assert on) plus the on-disk journal -- never internal Rust function names or
call structure. Any future capacity probe on the `>= 2` execute path (fail-closed
*or* warn-and-proceed) necessarily issues one of those two commands, so the
assertion is tied to the actual regression mechanism, not an incidental detail.

### Sketch

```rust
// Intent: a redundancy-preserving remove (3->2, remaining >= 2) issues NO
//   execute-time survivor-capacity probe -- the `>= 2` path is intentionally
//   not re-checked, unlike the fail-closed 2->1 path.
//
// Why it exists: the execute-time capacity re-check is gated on
//   `remaining == 1` (RemovePlan::execute) and is fail-closed by design; the
//   `>= 2` branch is warn-and-proceed and leans on `btrfs device remove`
//   ENOSPCing cleanly (see the EvictionCheck / check_eviction_space docstrings,
//   which say "Do not unify"). The positive half is pinned by
//   execute_rechecks_survivor_capacity_before_journal; this pins the negative
//   half. Without it, a refactor that "makes capacity checks consistent" by
//   adding a fail-closed re-check to the >= 2 execute path would refuse valid
//   removes on transient probe errors -- and, if placed after
//   journal::write_journal, also strand pending-op.json -- with no test to catch it.
//
// Scenario: an operator removes one disk from a healthy three-disk pool. Two
//   survivors remain, so execute proceeds from the pre-journal topology gate
//   straight to `btrfs device remove` with no survivor-capacity probe; the
//   journal is written and then cleared on success, nothing stranded.
#[test]
fn execute_skips_survivor_capacity_recheck_for_multi_survivor() {
    let f = PoolFixture::three_disk_healthy();
    let fs = MockFs::storage(vec![]);
    let params = f.remove_params().build(); // name="disk2", yes=true -> 3->2

    let plan_runner = RemovalPool::three_disk().install(MockRunner::default());
    let plan = plan_remove(&plan_runner, &fs, &params)
        .expect("plan succeeds on a healthy three-disk pool");

    // Separate fresh runner: requests() then captures ONLY the execute phase,
    // so the plan-time `>= 2` BtrfsDeviceUsageRaw probe is excluded by
    // construction. Mirrors execute_rechecks_survivor_capacity_before_journal.
    let exec_runner = RemovalPool::three_disk().install(MockRunner::default());
    plan.execute(&exec_runner, &fs, &params)
        .expect("3->2 execute succeeds on a healthy pool");

    let calls = exec_runner.requests();
    assert!(
        !calls.iter().any(|c| matches!(
            c,
            CmdRequest::BtrfsDeviceUsageRaw { .. } | CmdRequest::BtrfsFilesystemDfJson { .. }
        )),
        "the `remaining >= 2` execute path must not re-probe survivor capacity \
         (it relies on btrfs device remove ENOSPCing cleanly): {calls:?}"
    );
    assert!(
        calls
            .iter()
            .any(|c| matches!(c, CmdRequest::BtrfsDeviceRemove { .. })),
        "the 3->2 remove must actually reach btrfs device remove: {calls:?}"
    );
    assert!(
        !f.paths.pending_op_json().exists(),
        "a successful 3->2 remove must clear the journal -- nothing stranded",
    );
}
```

## Files to modify

- `cli/src/remove.rs` -- add the single test above, immediately after
  `execute_rechecks_survivor_capacity_before_journal` (~line 1450). No production
  code changes.

## Reuse (no new helpers)

All harness pieces already exist; the test introduces zero new fixtures:

- `PoolFixture::three_disk_healthy()` / `.remove_params()` -- `cli/src/test_fixtures/remove.rs:59,87`
- `RemovalPool::three_disk()` / `.install(...)` -- `cli/src/test_fixtures/remove.rs:124,134`
- `plan_remove` / `RemovePlan::execute` -- `cli/src/remove.rs:510`, `:241`
- `MockRunner` + `.requests()`, `CmdRequest` variants -- already imported in the
  test module (used by the positive test).

Deliberately **not** reusing `overcommitted_survivor_usage_stdout()` /
`overcommitted_survivor_df_json()` (`cli/src/test_fixtures/remove.rs:281,305`): they
are 2-disk-shaped (survivor=devid1, target=devid2) and purpose-built for the 2->1
refusal. Injecting them into a 3-disk execute runner would be topologically
inconsistent, and since the `>= 2` execute path never consults capacity at all, an
"over-committed-survivor still proceeds" variant adds no coverage over the direct
no-probe assertion while adding fixture surface. Healthy-runner + no-probe is the
simpler, stronger guard (it also catches a warn-and-proceed probe, which an
outcome-only assertion would miss).

## Verification

1. **Green on current code:**
   `cargo test execute_skips_survivor_capacity_recheck_for_multi_survivor`
   (or the full lane: `just test-rust`).
2. **Confirm it fails for the right reason (TDD discipline, per AGENTS.md):**
   temporarily drop the `== 1` from the execute gate (`cli/src/remove.rs:343`) so
   the survivor re-check runs on the `>= 2` path too, re-run the test, and confirm
   it goes **red** on the no-probe assertion (a `BtrfsDeviceUsageRaw` /
   `BtrfsFilesystemDfJson` now appears in the execute runner's requests). On the
   healthy `three_disk` execute runner the injected probe is served valid usage/df
   and *succeeds* (`preflight::check_single_survivor_capacity` passes,
   `cli/src/preflight.rs:408`), so `execute` still returns `Ok` -- the test reds
   **solely** because the no-probe assertion recorded the probe requests, not because
   the probe errors. The post-journal `pending-op.json` strand is a separate
   hypothetical the same assertion guards by construction: any capacity probe --
   success or error, pre- or post-journal -- shows up in the request stream. Revert.
3. Confirm the positive test `execute_rechecks_survivor_capacity_before_journal`
   still passes -- the pair must be green together.
