# Hold a sleep inhibitor during bootstrap-add recovery's owed RAID1 balance

## Context

`braid recover` replays the owed RAID1 soft balance for an interrupted
multi-disk bootstrap-add **without holding a sleep inhibitor** -- the one
balance-replay path in recover that violates the ADR 019 invariant.

Trace: a batch bootstrap (`braid add disk1 disk2` on an empty pool) journals
`OpKind::Add { phase: PoolMutation }` with empty `pre_membership`, so
`Journal::is_bootstrap_add()` is true. In the dispatch
(`cli/src/recover.rs`, the `match &journal.op` around recover.rs:1473) that
skips the `!is_bootstrap_add()` arm and the `PostAddBalanceRaid1` arm and
falls through to `OpKind::Add { .. } => GenericLivePool { replay_raid1_maintenance: true }`
(recover.rs:1538-1544). At execute time `execute_generic_live_pool_recovery`
runs:

```rust
if replay_raid1_maintenance {
    replay_owed_raid1_maintenance(runner, &plan.mount_point, "add", &pool, params.progress)?;
}
```

(recover.rs:1182-1184), which on a >=2-device live pool issues
`btrfs balance -dconvert=raid1,soft -mconvert=raid1,soft` via
`pool::pool_balance_raid1_soft` -- with **no `params.sleep_inhibitor.acquire(...)`
anywhere in the function**.

Every other balance-replay path acquires the inhibitor:
`execute_add_post_balance_recovery` (recover.rs:2304-2316),
`execute_remove_missing_post_maintenance_recovery` (recover.rs:2789-2802),
`execute_replace_post_maintenance_recovery` (recover.rs:3204-3241), and the
add-replay arm (recover.rs:2449-2452). The happy-path `cmd_add` holds one
across its bootstrap + balance (`cli/src/add.rs`, acquire at add.rs:1248).
ADR 019 (`docs/design/decisions/019-inhibit-sleep.md`) line 119: *"braid
recover follows the same boundary for replayed destructive work."* This path
doesn't.

**Severity is low but real:** `pool_bootstrap_mount_raid1` already lays down
RAID1 via `mkfs.btrfs -d raid1 -m raid1`, so the `,soft` replay skips every
already-RAID1 chunk and the data-loss window is tiny. But it is a genuine
invariant violation on a mutating recovery path, and -- per `AGENTS.md` --
"reach for the ideal, robust, simple, most correct solution."

**Outcome:** bring this path into compliance so the inhibitor invariant holds
for *all* recover balance replays, and lock it with structure-insensitive
tests.

## Decision: gate the inhibitor on `replay_raid1_maintenance` (Option 1)

Acquire the guard inside the existing `if replay_raid1_maintenance` block,
mirroring `execute_add_post_balance_recovery`. Do **not** add a
`pool.devices.len() >= 2` sub-condition (rejected Option 2).

Rationale:
- **ADR 019 line 104 already decided this tradeoff** for the analogous
  per-command path: acquire even when the soft balance turns out to be a
  no-op, because "the boundary rule [stays] simple" and the savings are
  "tiny on a NAS that is idle most of the time." The single-disk-bootstrap
  case (`replay_raid1_maintenance` true, but the balance internally skipped
  because the live pool has 1 device) is exactly that blessed no-op acquire.
- **Sibling consistency:** every other recover handler gates the inhibitor on
  the journaled "owes a balance" flag (`restore_raid1_after_commit`), never on
  a live `pool.devices.len()` recheck. `replay_raid1_maintenance` is the exact
  analog. Option 2 would make this the only handler using a bespoke gate.
- **No duplicated invariant:** the `pool.devices.len() >= 2` runtime guard
  stays the single source of truth inside `replay_owed_raid1_maintenance`
  (recover.rs:1859); Option 2 would copy it to a second site that can drift.

The finding's "keep single-disk-bootstrap inhibitor-free" wording is set aside
deliberately: it conflicts with ADR 019 line 104, which is the governing
authority.

## The fix

**File:** `cli/src/recover.rs`, fn `execute_generic_live_pool_recovery`
(the block at recover.rs:1182-1184).

Replace the bare replay call with an inhibitor-guarded one:

```rust
if replay_raid1_maintenance {
    let _guard = params
        .sleep_inhibitor
        .acquire("finishing interrupted add balance")
        .map_err(|e| {
            RecoverError::Failed(format!("could not acquire sleep inhibitor: {e}"))
        })?;
    replay_owed_raid1_maintenance(runner, &plan.mount_point, "add", &pool, params.progress)?;
}
```

Reuse, do not invent:
- `params.sleep_inhibitor` (`RecoverParams::sleep_inhibitor`, recover.rs:222,
  `&dyn AcquireSleepInhibitor`) -- same seam the other four handlers use.
- The error-mapping string `"could not acquire sleep inhibitor: {e}"` and the
  reason `"finishing interrupted add balance"` -- byte-identical to
  `execute_add_post_balance_recovery` (recover.rs:2310-2313).

Notes:
- **Guard scope:** the `_guard` lives to the end of the `if` block, covering
  the balance. The trailing ghost-acked cleanup (recover.rs:1186-1196) and
  `clear_journal` (recover.rs:1198) run after it drops -- those are
  sub-second braid-state file writes, not interruptible btrfs work, so they
  need no protection.
- **Fail-closed ordering is preserved:** `save_membership` already ran
  (recover.rs:1172) and `clear_journal` has not, so an acquire failure
  returns `RecoverError::Failed` with the journal intact -- recover re-runs
  idempotently. Same shape as the sibling handlers.
- **No new `inhibitor_already_held` parameter.** Unlike the post-maintenance
  handlers, `execute_generic_live_pool_recovery` is only ever reached directly
  from the top-level `RecoverCompletion::execute` dispatch (recover.rs:731-740)
  -- never chained from a PoolMutation handler that already holds a guard -- so
  it can acquire unconditionally within the `if`.
- **No dry-run impact:** the guard sits inside the execute-only path; dry-run
  renders via `render_recovery_tail` and never reaches it. The existing
  `recover_dry_run_does_not_acquire_sleep_inhibitor` test (recover.rs:12415)
  continues to hold.

## Tests (lock the invariant)

The three tests below are behavioral and structure-insensitive -- they assert
observable acquire/command/drop ordering and fail-closed state, never the
guard's code shape.

**Extend the test inhibitor to record guard lifetime first.** Today's
`RequestCountInhibitor` (recover.rs:3820) records only acquisition -- its guard
is `Box::new(())` -- so a regression that acquires *and drops the guard before*
`BtrfsBalanceRaid1Soft` would still pass an acquire-order assertion while
violating the ADR 019 "held for the full duration" invariant. Extend it:
- Return a real guard struct (not `Box::new(())`) that owns a `MockRunner`
  clone plus a shared `Rc<Cell<Option<usize>>>`, with a `Drop` impl that
  records `runner.requests().len()` at drop time. Feasible because `SleepGuard`
  is a blanket `impl<T> SleepGuard for T {}` (recover/inhibit) with no `Send`
  bound and the returned `Box<dyn SleepGuard>` is `'static`, so a non-`Send`
  `Rc`-backed guard is allowed.
- Add a `drop_request_count()` accessor next to the existing `acquire_count()`
  / `first_acquire_request_count()`. `MockRunner` clones share their request
  log (the recover.rs:8241-8290 ordering assertions already rely on this), so
  inhibitor and guard observe the same log. All four existing users
  (recover.rs:8241, 8422, 10259, 10520) read only `acquire_count()` /
  `first_acquire_request_count()`, never drop state, so the additive
  drop-recording guard and new `drop_request_count()` accessor leave them
  unaffected.

1. **`cmd_recover_bootstrap_add_replays_owed_raid1_maintenance`**
   (recover.rs:17252) -- the multi-disk bootstrap-add recovery test that today
   asserts only that the balance runs. Wire the extended inhibitor
   (`.sleep_inhibitor(&inhibitor)` on the `recover_params()` build) and assert:
   - `assert_eq!(inhibitor.acquire_count(), 1);`
   - Let `balance_index` = position of the `BtrfsBalanceRaid1Soft` request in
     `runner.requests()`. Assert
     `first_acquire_request_count().unwrap() <= balance_index` (acquired before
     the balance issued) **and** `drop_request_count().unwrap() > balance_index`
     (still held when the balance issued). Together these prove the inhibitor
     was held **across** the soft balance, not merely acquired near it.

2. **`recover_skips_balance_replay_for_remove`** (recover.rs:17157) -- the
   `OpKind::Remove` path (`replay_raid1_maintenance: false`). Wire the inhibitor
   and `assert_eq!(inhibitor.acquire_count(), 0);` so the `false` arm is pinned:
   a path that owes no balance must not acquire.

3. **New: bootstrap-add acquire-failure is fail-closed.** No existing
   `FailingInhibitor` test covers the `GenericLivePool` branch -- the five
   (recover.rs:8786 / 9688 / 9727 / 9943 / 11977) cover add-pool-mutation,
   add-post-balance, remove-missing, and replace. Add one for bootstrap-add
   `cmd_recover`, mirroring
   `post_add_inhibitor_failure_stops_before_balance_and_preserves_journal`
   (recover.rs:9727): same probe mocks as test 1 (mountpoint + `btrfs
   filesystem show` two disks + cryptsetup status/uuid for both), but **no
   balance mocks** and `.sleep_inhibitor(&FailingInhibitor)` (the helper already
   exists, recover.rs:3812). The acquire is the first statement inside the `if`
   block, before `replay_owed_raid1_maintenance`, so the failure short-circuits
   every balance call. Assert:
   - the returned `Err` message contains `could not acquire sleep inhibitor`,
   - no `BtrfsBalanceStatus` and no `BtrfsBalanceRaid1Soft` request was issued,
   - `pending_op_json()` still exists (pool.json may already be written; the
     journal must survive for an idempotent re-run).

Explicitly **out of scope:** a test asserting single-disk bootstrap recovery
acquires-then-skips. That would pin the Option-1 no-op acquire, an
implementation choice rather than a safety behavior -- structure-sensitive, so
omit it.

## Docs

One sentence in `docs/design/decisions/019-inhibit-sleep.md`, in the `braid
recover` paragraph (around line 119), noting that the bootstrap-add
(`GenericLivePool`) replay also acquires the inhibitor for its owed RAID1 soft
balance -- so the doc enumerates the path that was missing it and future
readers see the invariant covers all recover balance replays. (The invariant
itself is unchanged; this is closing the documentation gap that let the
omission hide.)

## Verification

- Focused iteration: `cargo test --manifest-path cli/Cargo.toml --lib
  recover::tests` (the lib package is `braid-cli`, not `braid`; `cargo test -p
  braid` fails, as `justfile#test-rust` calls out). Final Rust lane: `just
  test-rust`. The three tests above plus the full recover suite must pass.
  Before implementing, optionally confirm test 1 fails on today's code with
  only the new assertions added (proving it actually guards the bug): the
  `acquire_count()` assertion goes from 0 to 1.
- `just clippy` (expanded: `cargo clippy --manifest-path cli/Cargo.toml
  --tests`) -- the new closure + `?` should be clean.
- `scripts/docs/check-output-ascii.py` and `just docs-build` if the ADR
  sentence is added (link/ascii checks).
- The inhibitor seam is unit-tested via the drop-recording inhibitor and
  `FailingInhibitor`; a NixOS VM test cannot observe logind inhibitor
  acquisition, so no VM test is added.

## Files to modify

- `cli/src/recover.rs` -- the fix in `execute_generic_live_pool_recovery`; the
  `RequestCountInhibitor` drop-recording extension; two edited tests
  (bootstrap-add lifetime, remove no-acquire) and one new fail-closed test.
- `docs/design/decisions/019-inhibit-sleep.md` -- one clarifying sentence.
