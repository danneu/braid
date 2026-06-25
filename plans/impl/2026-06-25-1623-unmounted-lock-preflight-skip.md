# Plan: guard that an unmounted `lock` skips the exclusive-op preflight

## Context

`plan_lock` (`cli/src/lock.rs`) decides `pause_balance_before_unmount` by routing
through a `Snapshot` enum. Two arms consult the FSID-keyed exclusive-operation
preflight (`lock_preflight_pause_decision`):

- `Snapshot::Probed(pool)` -- only `if let Some(fsid) = &pool.fsid`.
- `Snapshot::ProbeFailed { fsid, .. }` -- always.

The third arm, `Snapshot::Unmounted` (`cli/src/lock.rs:901-903`), deliberately
**bypasses** the preflight and calls `build_close_sets_uuid_scanned_fallback`
directly, leaving `pause_balance_before_unmount` at its `false` default
(`lock.rs:883`). "Skip the exclusive-op preflight when the pool is not mounted"
is an explicit documented invariant:

- `docs/commands/lock.md` step 2: "Checks that no btrfs exclusive operation
  (balance, device remove, etc.) is running. Skipped when the pool is not mounted."
- ADR-024 item 7 (`docs/design/decisions/024-luks-uuid-identity.md`): the FSID is
  read to key the preflight only on the *mounted* probe-failure path.

**The gap:** no unit test (and no NixOS VM test) pins this bypass. Every existing
`with_excl_op(...)` test is on a *mounted* arm; every *unmounted* test leaves
`excl_op` at its `"none"` default. So a refactor that routed the `Unmounted` arm
through the preflight (e.g. by re-probing an FSID and sharing the `ProbeFailed`
arm) would regress silently -- with `"none"` seeded, the regression still yields
`pause == false`, so every current test stays green. The harm is concrete:

- **User mode:** the unmounted lock would *spuriously fail* during a (stale)
  balance reading, because `require_lock_preflight` returns `Err`.
- **SystemdStop mode:** `pause_balance_before_unmount` would flip to `true`, so
  ExecStop would try to pause a balance on a pool that isn't even mounted.

This area was just refactored (`fb99872b` centralized the preflight dispatch into
`lock_preflight_pause_decision`; `680f6ba8` introduced the `Snapshot` enum), which
is exactly when a behavioral regression guard earns its keep.

## Fix

Add **two self-contained Rust unit tests** to `mod tests` in `cli/src/lock.rs`,
one per `LockMode`. Each seeds the filesystem with a stale `exclusive_operation`
reading and asserts the unmounted plan ignores it.

Two fixtures together make the regression proof *discriminating*:

- **Runner wrapped with `lock_with_fsid_probe_mocks(...)`.** The only plausible way
  to route the `Unmounted` arm through the preflight is to first obtain an FSID
  (re-probe, mirroring `ProbeFailed`). `probe_fsid` issues `BtrfsFilesystemShow`
  (`cli/src/probe.rs:564`); with only `MountpointCheck` mocked, that hits
  `MockRunner` `MissingMock` and the regression dies *before* the preflight ever
  reads the seeded op -- so the test would catch the regression only via an
  incidental panic, proving nothing about the seed. Arming the FSID probe lets the
  regression's `probe_fsid` succeed and advance into `lock_preflight_pause_decision`,
  where the seed actually bites. `lock_fs(&[])` reports `/mnt/storage` as btrfs in
  mountinfo, so `probe_fsid`'s `fstype_at_mount_via_fs` check does not early-return.
- **`with_excl_op("balance")`.** This is what the preflight then reads. With the
  default `"none"` the regression yields `pause == false` (and user mode does not
  error), so the test would pass even with the bug. An active op is what makes the
  skip-vs-run behaviors observably diverge: user mode errors on the op, systemd-stop
  sets `pause_balance_before_unmount = true`.

The runner mocks the FSID probe even though the *correct* `Unmounted` path never
calls it; those mocks exist solely to arm the bite proof. The current (correct)
test passes without consuming them, and `lock_with_fsid_probe_mocks` adds outputs
only (it leaves `MountpointCheck` at exit 1), so `pool_was_mounted` stays `false`.

The two tests are written as independent, fully-composed functions (no shared
helper / no parametrization) to match the project's flat-fixture philosophy
(`cli/src/test_fixtures/lock.rs` header: "the fixture stays flat so individual
tests still compose the precise request set they intend to prove"). The codebase
has no `rstest`/parametrized-test precedent.

### Test 1 -- `unmounted_user_lock_ignores_active_exclusive_op`

```rust
// Intent: an unmounted user lock does not consult the exclusive-op preflight;
//   a stale `balance` reading must not make the plan refuse.
// Why it exists: "skip the preflight when not mounted" is an invariant
//   (ADR-024 item 7; lock.md step 2). The Snapshot::Unmounted arm bypasses
//   lock_preflight_pause_decision entirely. A refactor routing it through the
//   user preflight (e.g. by re-probing an FSID) would make an unmounted lock
//   spuriously fail during a balance -- and every existing test would stay
//   green because all unmounted tests leave exclusive_operation at "none".
//   In user mode that regression surfaces as the `.expect()` below panicking
//   (the preflight returns Err, so plan_lock fails), not as the pause
//   assertion failing; the pause assertion is the direct discriminator in the
//   systemd-stop sibling test, where the preflight returns Ok(true).
// Scenario: pool is not mounted but sysfs still reports a stale `balance`
//   exclusive op (a state only constructible in a unit test); operator runs
//   `braid lock`.
#[test]
fn unmounted_user_lock_ignores_active_exclusive_op() {
    // FSID-probe mocks armed so a re-probe regression reaches the preflight
    // (and bites on the seeded op) instead of dying on a missing BtrfsFilesystemShow.
    let runner = lock_with_fsid_probe_mocks(MockRunner::default().with_output(
        CmdRequest::MountpointCheck {
            path: MountPoint::new("/mnt/storage".to_owned()).into(),
        },
        lock_err_raw("mountpoint -q /mnt/storage", 1, ""),
    ));
    let fs = lock_fs(&[]).with_excl_op("balance");
    let config = lock_test_config();
    let membership = lock_test_membership();

    let plan = plan_lock(&runner, &fs, &config, &membership, LockMode::User)
        .expect("unmounted lock must plan without consulting the exclusive-op preflight");

    assert!(!plan.pool_was_mounted, "test must exercise the Unmounted arm");
    assert!(
        !plan.pause_balance_before_unmount,
        "unmounted lock must not pause for a balance: the preflight is skipped when not mounted"
    );
}
```

### Test 2 -- `unmounted_systemd_stop_lock_ignores_active_exclusive_op`

Identical setup, but `LockMode::SystemdStop`. This is the mode where
`pause_balance_before_unmount` is the *directly discriminating* assertion: a
regression would flip it to `true` (SystemdStop's preflight returns `Ok(true)`
for a running balance) while the plan still succeeds.

```rust
// Intent: same invariant under the systemd-stop preflight contract -- an
//   unmounted ExecStop lock does not pause for a stale running balance.
// Why it exists: SystemdStop's preflight returns Ok(true) for a running
//   balance, so routing the Unmounted arm through it would set
//   pause_balance_before_unmount = true and make ExecStop try to pause a
//   balance on a pool that is not mounted -- silently, with all tests green.
// Scenario: shutdown ExecStop runs lock on an already-unmounted pool while
//   sysfs still reports a stale `balance` op.
#[test]
fn unmounted_systemd_stop_lock_ignores_active_exclusive_op() {
    // FSID-probe mocks armed so a re-probe regression reaches the preflight
    // (and bites on the seeded op) instead of dying on a missing BtrfsFilesystemShow.
    let runner = lock_with_fsid_probe_mocks(MockRunner::default().with_output(
        CmdRequest::MountpointCheck {
            path: MountPoint::new("/mnt/storage".to_owned()).into(),
        },
        lock_err_raw("mountpoint -q /mnt/storage", 1, ""),
    ));
    let fs = lock_fs(&[]).with_excl_op("balance");
    let config = lock_test_config();
    let membership = lock_test_membership();

    let plan = plan_lock(&runner, &fs, &config, &membership, LockMode::SystemdStop)
        .expect("unmounted systemd-stop lock must plan without consulting the preflight");

    assert!(!plan.pool_was_mounted, "test must exercise the Unmounted arm");
    assert!(
        !plan.pause_balance_before_unmount,
        "unmounted systemd-stop lock must not pause for a balance: preflight is skipped when not mounted"
    );
}
```

### Notes on the test body

- `lock_fs(&[])` (empty mapper set) keeps the close-set side trivial: the correct
  `Unmounted` path scans an empty `/dev/mapper`, issues no `cryptsetup` calls, and
  leaves the per-device mocks inside `lock_with_fsid_probe_mocks` unused. The
  `Unmounted` arm computes `pause_balance_before_unmount` independently of close-set
  contents, so an empty close set does not weaken the assertion.
- The runner is wrapped with `lock_with_fsid_probe_mocks` solely to arm the bite
  proof (see "Fix"); only its `BtrfsFilesystemShow` response is load-bearing there.
  Because the correct `Unmounted` path never calls `probe_fsid`, these mocks are
  never consumed in a green run, and `MockRunner` has no strict-consumption
  assertion -- so they read as "unused" to a maintainer. Ship the inline
  `// FSID-probe mocks armed ...` comment **verbatim**: it is the only thing
  stopping a future "prune dead mocks" cleanup from dropping the wrap and silently
  downgrading the guard. (Without the wrap the regression would still fail, but via
  a `MissingMock` panic on `BtrfsFilesystemShow` -- losing the proof that the
  seeded *active op*, not an incidental panic, is what catches it.)
- The `MountpointCheck` -> exit 1 mock is what drives `pool_was_mounted == false`
  and thus the `Snapshot::Unmounted` arm (the fs mountinfo is not consulted for
  this); the `assert!(!plan.pool_was_mounted)` line self-validates that the test
  exercises the intended path.
- `pause_balance_before_unmount` is a private field of `LockPlan`, readable from
  `mod tests` (same module); precedent at `cli/src/lock.rs:3096`.

### Placement

Add both tests adjacent to `systemd_stop_probe_failed_fallback_pauses_running_balance`
(`cli/src/lock.rs:3062`) so the contrast reads top-to-bottom: *mounted
ProbeFailed pauses a balance* immediately followed by *unmounted ignores one*.
Exact line is not load-bearing.

## Considered and deferred: the `Probed { fsid: None }` sibling skip-path

`Snapshot::Probed(pool)` also skips the preflight when `pool.fsid == None`
(`cli/src/lock.rs:885-891`). **Deliberately not guarded here.** Rationale:

- It is reachable only via a TOCTOU race -- the pool unmounts between
  `MountpointCheck` and `probe_pool`, so `probe_pool` early-returns `fsid: None`
  (`cli/src/probe.rs:425-435`). It is not the documented "not mounted" invariant.
- It is already structurally safe two ways: the `if let Some(fsid)` guard plus the
  `false` default.
- A test would have to fabricate a mount/probe disagreement to reach the state --
  structure-sensitive scaffolding that pins an internal race artifact with no
  stated contract, which conflicts with the "behavioral, structure-insensitive"
  test bar. If this path ever needs coverage, it belongs alongside the
  probe-race tests in `cli/src/probe.rs`, not this finding.

## Files to modify

- `cli/src/lock.rs` -- add the two `#[test]` functions to `mod tests`. They reuse
  the existing `lock_with_fsid_probe_mocks` fixture (already imported into the test
  module at `cli/src/lock.rs:1317`). No production code, fixture, or docs change
  (the invariant is already documented in `lock.md` and ADR-024; the tests just
  pin it).

## Verification

1. `just test-rust` (or `cargo test -p braid lock::tests`) -- both new tests pass.
2. **Confirm they bite (do this during implementation, revert after):**
   - Temporarily change the `Unmounted` arm (`cli/src/lock.rs:901-903`) to mirror
     `ProbeFailed`: `probe_fsid(...)` then `lock_preflight_pause_decision(fs, mode, &fsid)`.
   - With the FSID-probe mocks armed, `probe_fsid` succeeds, so the regression
     reaches the preflight and bites for the intended reason:
     `unmounted_systemd_stop_lock_ignores_active_exclusive_op` fails on the
     `!plan.pause_balance_before_unmount` assertion (systemd-stop returns `Ok(true)`
     for the active op), and `unmounted_user_lock_ignores_active_exclusive_op` fails
     because the user preflight returns `Err`, so `plan_lock` errors and `.expect`
     panics.
   - Prove the seed -- not an incidental panic -- is what catches the regression:
     with the mutation still in place, flip `with_excl_op("balance")` to `"none"`;
     both tests should now PASS (no op for the preflight to act on). Restore
     `"balance"`.
   - Revert the mutation; both tests green again.
3. No fixture-refresh event (no `flake.lock`/pinned-package change), so the parser
   fixture lanes are untouched.
