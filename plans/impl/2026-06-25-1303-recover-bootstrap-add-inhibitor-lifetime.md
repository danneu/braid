# Plan: hold a sleep inhibitor across the generic-path bootstrap RAID1 balance replay

## Context

`braid recover` replays an owed RAID1 soft balance after several interrupted
operations. Every replay path acquires a logind sleep inhibitor around the
destructive work so the machine cannot suspend mid-operation -- **except one**.

`execute_generic_live_pool_recovery` (`cli/src/recover.rs#execute_generic_live_pool_recovery`)
calls `replay_owed_raid1_maintenance` with **no** sleep inhibitor, while its three
sibling replay paths all guard the call:

| Call site | Guards replay? |
|---|---|
| `execute_generic_live_pool_recovery` (~line 1183) | **NO -- the bug** |
| `execute_add_post_balance_recovery` (~line 2316) | yes ("finishing interrupted add balance") |
| `execute_remove_missing_post_maintenance_recovery` (~line 2802) | yes ("...remove-missing maintenance") |
| `execute_replace_post_maintenance_recovery` (~line 3241) | yes ("...replace maintenance"), also covers the preceding resize |

This generic path is reached for bootstrap-add recovery (`OpKind::Add` +
`is_bootstrap_add()` routes to `RecoverCompletion::GenericLivePool { replay_raid1_maintenance: true }`;
the Remove arm routes here with `false`). So a 2-disk bootstrap that crashed
after `mkfs.btrfs` replays its RAID1 soft balance with the machine free to
suspend.

Severity is low (the balance runs on a freshly-created, near-empty pool and
btrfs balance is suspend/crash-safe), but the omission **violates documented
policy**: [ADR 019 #current-application](docs/design/decisions/019-inhibit-sleep.md#current-application)
states that `braid recover` "follows the same boundary for replayed destructive
work" and that the suite acquires the inhibitor "immediately before" the
destructive work and "holds it until the function returns." The generic
bootstrap-add replay is the lone recover code path that does not. And,
critically, **no test catches it**: `bootstrap_recovery_clears_acked_stats`
uses the default `NoopInhibitor` and asserts only acked-stats deletion;
`cmd_recover_bootstrap_add_replays_owed_raid1_maintenance` asserts the balance is
issued + journal cleared but never the inhibitor; the only VM test
(`tests/cli/recover-bootstrap-crash.py`) is single-disk and stops at the pre-mkfs
NoBtrfs escape path.

Intended outcome: the generic bootstrap-add replay holds an inhibitor exactly
like its siblings, locked in by a regression test that fails today and passes
after the fix.

## Approach: local consistent fix (mirror the sibling pattern)

The inhibitor is an *operational* guard (don't suspend mid-balance), not a
data-safety invariant, and braid's established pattern is uniform: each
destructive recovery tail acquires `let _guard` at its **call site**. The fix
restores that uniformity rather than centralizing acquisition into the helper
(which would make one destructive helper self-acquire while all others stay
caller-managed, change a fail-closed helper's signature, and perturb the tested
add/remove/replace paths -- see "Rejected alternative").

### 1. Production fix -- `cli/src/recover.rs#execute_generic_live_pool_recovery`

Bind a sleep-inhibitor guard as an `Option<_>` at **function-body scope**
(immediately after `membership::save_membership` / the bootstrap acked-stats
removal, and immediately before the replay), copying the idiom verbatim from the
sibling post-maintenance paths -- e.g. `execute_remove_missing_post_maintenance_recovery`
(`cli/src/recover.rs:2789-2810`), whose `_guard` lives through `journal::clear_journal`:

```rust
// Hold the inhibitor from immediately-before-replay through the journal-clear
// tail, matching the sibling post-maintenance paths and ADR 019's
// "holds it until the function returns".
let _guard = if replay_raid1_maintenance {
    Some(
        params
            .sleep_inhibitor
            .acquire("finishing interrupted add balance")
            .map_err(|e| RecoverError::Failed(format!("could not acquire sleep inhibitor: {e}")))?,
    )
} else {
    None
};
if replay_raid1_maintenance {
    replay_owed_raid1_maintenance(runner, &plan.mount_point, "add", &pool, params.progress)?;
}
// existing Remove ghost-acked block + journal::clear_journal now run under _guard
```

Notes:
- **Guard lifetime mirrors the siblings (F1).** The binding is hoisted out of the
  replay `if` as `Option<_>` so that for `replay_raid1_maintenance == true` it is
  held through the ghost-acked tail and `journal::clear_journal`, matching
  `recover.rs:2789-2810` and ADR 019's "acquire ... immediately before ... holds
  it until the function returns." Do *not* scope it to the `if` block (that would
  drop it before journal clear and diverge from the suite).
- The Remove arm (`replay_raid1_maintenance == false`) binds `None`: that arm does
  no destructive btrfs work (membership save + journal clear are local file ops),
  so there is nothing to inhibit -- consistent with the sibling's
  `if ... !restore_raid1_after_commit { None }` gate.
- No `inhibitor_already_held` plumbing: the dispatch chain to this function never
  pre-holds an inhibitor (confirmed -- `cmd_recover` -> `RecoverWorkPlan::execute`
  -> `RecoverCompletion::execute` GenericLivePool arm holds none), so a bare
  `Option` guard is correct.
- Reason string matches the nearest sibling (`execute_add_post_balance_recovery`);
  both are the post-add RAID1 balance.

### 2. Regression tests -- `cli/src/recover.rs` test module

Reuse the existing `RequestCountInhibitor` (`cli/src/recover.rs:3820`), the
`.sleep_inhibitor(&...)` builder (`cli/src/test_fixtures/recover.rs`), and the
ordering-assertion idiom already used by the add-recovery tests
(`acquire_count()`, `first_acquire_request_count()`; see ~lines 8283-8300).
Each test gets the standard `// Intent / Why it exists / Scenario` preamble.

**Positive (`bootstrap_recovery_holds_inhibitor_across_balance`)** -- mirrors the
setup of `bootstrap_recovery_clears_acked_stats` but threads a counting
inhibitor:
- `let runner = with_balance_replay(MockRunner::default());`
- `let inhibitor = RequestCountInhibitor::new(runner.clone());`
- `let params = f.recover_params().passphrase_file(None).sleep_inhibitor(&inhibitor).build();`
- call `execute_generic_live_pool_recovery(&runner, &resolver, &params, &plan, pool_state_two_disks(), true)`
- assert `inhibitor.acquire_count() == 1`
- assert the `BtrfsBalanceRaid1Soft` request index `>= inhibitor.first_acquire_request_count().unwrap()`
  (balance runs inside the inhibitor window).

  This is the test that **fails today** (`acquire_count()` is 0) and passes after
  the fix.

**Negative (`generic_recovery_remove_arm_does_not_acquire_inhibitor`)** -- proves
the non-replay arm stays inhibitor-free:
- build a 2-disk-live Remove journal (reuse `remove_2to1_journal_with_target_devid`
  at `cli/src/recover.rs:17327` or an equivalent 2-disk-live remove journal) via
  `recover_work_plan_for_journal`
- `let inhibitor = RequestCountInhibitor::new(runner.clone());` threaded via
  `f.recover_params().passphrase_file(None).sleep_inhibitor(&inhibitor).build()`
- call `execute_generic_live_pool_recovery(..., pool_state_two_disks(), false)`
- assert `inhibitor.acquire_count() == 0`.

  Passes before and after the fix; guards against a future change that acquires
  unconditionally.

**Failure path (`bootstrap_recovery_inhibitor_failure_aborts_before_balance`)** --
proves the `?`/`map_err` propagation is wired, not merely that `acquire` is
called. A fix that calls `acquire` but swallows its error would still pass the
happy-path counter test yet let the balance run without a valid inhibitor; this
test closes that hole. Mirrors `post_add_inhibitor_failure_stops_before_balance_and_preserves_journal`
(`cli/src/recover.rs` ~9720) and reuses the existing `FailingInhibitor`
(`cli/src/recover.rs:3812`):
- `let inhibitor = FailingInhibitor;`
- bootstrap journal + `with_balance_replay(MockRunner::default())`, replay=true
- `let params = f.recover_params().passphrase_file(None).sleep_inhibitor(&inhibitor).build();`
- call `execute_generic_live_pool_recovery(..., pool_state_two_disks(), true)` and
  expect `Err`
- assert the error string contains `could not acquire sleep inhibitor`
- assert `runner.requests()` contains no `CmdRequest::BtrfsBalanceStatus { .. }`
  and no `CmdRequest::BtrfsBalanceRaid1Soft { .. }` (acquire failure aborts before
  `replay_owed_raid1_maintenance`, which issues both -- same assertion shape as
  recover.rs:8830 / 9751 / 11091)
- assert `f.paths.pending_op_json().exists()` (journal preserved for retry).

  Fails before the fix (today `acquire` is never called, so the balance runs and
  the journal clears with no error) and passes after.

**Optional end-to-end strengthening** -- augment the existing
`cmd_recover_bootstrap_add_replays_owed_raid1_maintenance` (`cli/src/recover.rs:17252`)
to pass a `RequestCountInhibitor` and assert `acquire_count() == 1`, giving
coverage at the real `cmd_recover` entry point in addition to the direct-call
unit test. Recommended but secondary; the direct positive test is the core
requirement.

## Reused existing code (no new abstractions)

- Sibling guard idiom: `cli/src/recover.rs#execute_add_post_balance_recovery` (~2304-2316).
- `replay_owed_raid1_maintenance` (`cli/src/recover.rs:1823`) -- unchanged; already
  owns the fail-closed balance-state checks and the `pool.devices.len() >= 2` gate.
- Test seams: `RequestCountInhibitor` (`recover.rs:3820`), `with_balance_replay`
  (`recover.rs:5688`), `pool_state_two_disks` (`recover.rs:5245`),
  `recover_work_plan_for_journal`, `remove_2to1_journal_with_target_devid`
  (`recover.rs:17327`), and the `RecoverParamsBuilder::sleep_inhibitor` builder
  (`cli/src/test_fixtures/recover.rs`, default `NoopInhibitor`).

## Docs

No doc edit required -- the change makes the code conform to behavior the docs
already describe (the code was the outlier, not the docs):

- [ADR 019 #current-application](docs/design/decisions/019-inhibit-sleep.md#current-application)
  already states `braid recover` holds a sleep inhibitor for replayed destructive
  work: add `PoolMutation` recovery acquires it after reversible credential
  checks, immediately before replaying btrfs work, and holds it until the function
  returns. The fix brings the generic bootstrap-add path into line with this
  documented boundary.
- [balance-soft.md #recover-replay](docs/internals/btrfs/balance-soft.md#recover-replay)
  already documents the owed-RAID1 `replay_owed_raid1_maintenance` that the
  inhibitor protects.

Only touch these docs if the implementation intentionally changes the documented
boundary -- it does not.

## Verification

1. `just test-rust` (or `cargo test` scoped to the recover module).
   - TDD: write the tests first and watch them fail for the right reason, then
     apply the fix. Two tests **fail before** the production edit:
     `bootstrap_recovery_holds_inhibitor_across_balance` (`acquire_count` 0,
     expected 1) and `bootstrap_recovery_inhibitor_failure_aborts_before_balance`
     (today no `acquire` is called, so the balance runs and the journal clears
     with no error).
   - After the fix: the positive test, the failure-path test, the negative arm
     test (`acquire_count == 0`, green both before and after), and all existing
     inhibitor/acked-stats tests pass.
2. Confirm no behavior change on the other replay paths (their call sites and the
   `replay_owed_raid1_maintenance` signature are untouched), so the existing
   add/remove/replace inhibitor tests stay green unchanged.
3. CLI output ASCII check still passes (`scripts/docs/check-output-ascii.py`) --
   the reason string is plain ASCII; no `echo`/output lines added.

## Rejected alternative: centralize acquisition into `replay_owed_raid1_maintenance`

Move the `acquire` inside the helper (gated on `pool.devices.len() >= 2`),
threading `inhibitor: &dyn AcquireSleepInhibitor` + `inhibitor_already_held`.
Rejected because:
- It makes `replay_owed_raid1_maintenance` the **only** destructive helper that
  self-acquires, while `pool_add_device`, `pool_resize_device`,
  `ensure_keyfile_enrolled`, `backup_luks_header` stay caller-managed -- trading
  the current uniform "caller-acquires-around-destructive-tail" pattern for a new
  inconsistency.
- It only partially dedups: the replace path must still acquire early to cover
  its `pool_resize_device` and pass `already_held=true`; the three non-replay
  acquire sites are unaffected.
- It changes a fail-closed safety helper's signature and shifts acquire-count
  semantics (acquire now tracks the `>= 2` gate) across add/remove/replace --
  meaningful blast radius and test churn for a low-severity bootstrap-only gap.
- The inhibitor is an operational guard, not a data-safety invariant, so the
  "place the invariant at an unbypassable chokepoint" argument (safety-heuristics)
  does not apply here.

## Implementation notes

- Current HEAD already contained the earlier command-level bootstrap-add
  inhibitor fix from `f98f13f3`, so the direct acquire/failure tests added here
  did not fail against the starting tree; this implementation still adds the
  direct helper coverage and hoists the guard to function-body scope as planned.
