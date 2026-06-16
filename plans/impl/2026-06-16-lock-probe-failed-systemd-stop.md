# Plan: close the `ProbeFailed` + `SystemdStop` coverage gap in lock teardown

## Context

`plan_lock` (`cli/src/lock.rs`) chooses a lock-teardown preflight by matching on
two axes: the mounted-pool `Snapshot` (`Probed` vs `ProbeFailed`) and the
`LockMode` (`User` vs `SystemdStop`). The `User` branch calls
`require_lock_preflight` (rejects *any* running exclusive op); the `SystemdStop`
branch calls `systemd_stop_lock_requires_balance_pause` (permits a running
balance and returns whether to pause it before unmount, so shutdown can lock the
pool mid-balance instead of leaving LUKS open).

That `match mode { User => ...; SystemdStop => ... }` block is **copy-pasted in
both arms** -- the `Probed` arm at `cli/src/lock.rs:898-907` and the
`ProbeFailed` arm at `cli/src/lock.rs:916-925`. Of the four resulting cells, one
is entirely untested:

| Snapshot \ Mode | `User`                                            | `SystemdStop`                          |
| --------------- | ------------------------------------------------- | -------------------------------------- |
| `Probed`        | covered (many `plan_lock` tests)                  | covered (`systemd_stop_*` cmd tests)   |
| `ProbeFailed`   | covered (`mounted_probe_failure_fallback_*`, VM)  | **uncovered**                          |

The `ProbeFailed` fallback is real in production -- btrfs can report a
non-`/dev/mapper/` device path for braid's own mounted pool -- and is proven for
`User` mode by `tests/cli/braid-lock-probe-failed.py`. But it is never combined
with the shutdown path. A regression that routed the `ProbeFailed`+`SystemdStop`
branch to `require_lock_preflight` would make ExecStop **refuse to lock during a
balance and leave LUKS open across shutdown**, while every existing test stays
green (the `Probed`-arm and `User`-arm tests all exercise a different cell).

Intended outcome: make that wrong-gate regression *structurally impossible* and
add the behavioral guard braid's own test doctrine prescribes.

## Approach

Two coordinated changes, both endorsed by `docs/dev/testing.md` "VM and command
test design": *"For `cmd_*` boolean gates derived from multiple inputs, route
both branches through the same injected seam and test the matrix cells that
distinguish the intended gate from plausible wrong gates."*

### Part 1 -- dedup the dispatch into one seam (root cause)

Extract the duplicated `match mode` dispatch into a single private helper in
`cli/src/lock.rs`, and call it from both arms. `pause_balance_before_unmount`
then has exactly one source of truth, so the `Probed` and `ProbeFailed` arms can
no longer drift.

```rust
/// Single source of truth for the lock-teardown preflight + balance-pause
/// decision, shared by the `Probed` and `ProbeFailed` arms so the
/// `User`/`SystemdStop` policy cannot drift between them. Returns whether a
/// running balance must be paused before unmount (`User` never pauses; it
/// hard-fails on any active exclusive op instead).
fn lock_preflight_pause_decision<F: Filesystem + ?Sized>(
    fs: &F,
    mode: LockMode,
    fsid: &Fsid,
) -> Result<bool, String> {
    match mode {
        LockMode::User => preflight::require_lock_preflight(fs, fsid).map(|()| false),
        LockMode::SystemdStop => preflight::systemd_stop_lock_requires_balance_pause(fs, fsid),
    }
}
```

Call sites collapse to one line each:

```rust
// Snapshot::Probed(pool) arm -- keeps its Option<fsid> guard:
if let Some(fsid) = &pool.fsid {
    pause_balance_before_unmount =
        lock_preflight_pause_decision(fs, mode, fsid).map_err(LockError::Failed)?;
}

// Snapshot::ProbeFailed { fsid, .. } arm -- fsid is always present:
pause_balance_before_unmount =
    lock_preflight_pause_decision(fs, mode, fsid).map_err(LockError::Failed)?;
```

This is behavior-preserving (the `User` arm still yields `false`, which matches
today's `pause_balance_before_unmount` default) and consistent with
`docs/dev/safety-heuristics.md`: the caller policy gate stays at the `plan_lock`
callsite layer, and the fail-closed policy is unchanged (same two preflight
functions, same rejection semantics).

### Part 2 -- the matrix-cell test (behavioral guard)

Add one Rust unit test in `cli/src/lock.rs`'s `mod tests` that drives the
**uncovered cell** at `plan_lock` level -- the layer that owns the decision --
and *distinguishes the intended gate from the wrong gate*. Model it on the
sibling `mounted_probe_failure_fallback_closes_uuid_verified_member` (probe-
failure setup), extended to two mappers, `with_excl_op("balance")`, and
`LockMode::SystemdStop`.

```rust
// Intent: a systemd-stop lock under the mounted ProbeFailed fallback pauses a
//   running balance before teardown instead of refusing it.
// Why it exists: the ProbeFailed arm carries its own LockMode dispatch; a
//   regression routing it to require_lock_preflight (rejects any running
//   balance) would make ExecStop refuse to lock during a balance and strand
//   LUKS open across shutdown, yet every Probed-arm and User-arm test stays
//   green. The User-mode fallback is proven real by
//   tests/cli/braid-lock-probe-failed.py; this pins the shutdown variant.
// Scenario: btrfs reports a non-/dev/mapper/ path for braid's own mounted pool
//   (modeled by a per-device probe failure) while a UPS low-battery shutdown
//   interrupts a running balance and ExecStop runs lock cleanup.
#[test]
fn systemd_stop_probe_failed_fallback_pauses_running_balance() {
    let runner = lock_with_fsid_probe_mocks(MockRunner::default().with_output(
        CmdRequest::MountpointCheck {
            path: MountPoint::new("/mnt/storage".to_owned()),
        },
        lock_ok_raw("mountpoint -q /mnt/storage"),
    ))
    .with_output_sequence(
        CmdRequest::CryptsetupStatus {
            mapper: MapperName::from_basename("braid-aaa".into()),
        },
        vec![
            lock_err_raw("cryptsetup status braid-aaa", 5, "transient status failure"),
            cryptsetup_status_active("braid-aaa", "/dev/disk/by-id/a"),
        ],
    );
    let fs = lock_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"])
        .with_excl_op("balance");
    let config = lock_test_config();
    let membership = lock_test_membership();

    let plan = plan_lock(&runner, &fs, &config, &membership, LockMode::SystemdStop)
        .expect("probe-failed fallback should plan under systemd-stop");

    // Distinguishes the cell: only the ProbeFailed arm pushes this warning.
    assert!(
        plan.notes.iter().any(|note| matches!(
            note,
            PreviewNote::Warn(body)
                if body.contains("falling back to UUID-scanned mapper cleanup")
        )),
        "expected the ProbeFailed fallback warning, got: {:?}",
        plan.notes,
    );
    // The gate under test: pause (not reject) a running balance.
    assert!(
        plan.pause_balance_before_unmount,
        "systemd-stop fallback must pause a running balance before unmount"
    );
    // Both UUID-verified members land in the teardown set.
    assert_eq!(
        member_summaries(&plan.close_set),
        vec![
            ("braid-aaa".to_owned(), "aaa".to_owned()),
            ("braid-bbb".to_owned(), "bbb".to_owned()),
        ],
    );
    assert!(!plan.cleanup_uncertain);
}
```

**Why `plan_lock` level, not end-to-end `cmd_lock_systemd_stop`:** for a healthy
two-member pool the `Probed` and `ProbeFailed` arms emit an *identical* command
sequence, so an end-to-end test cannot prove which arm ran -- it could silently
pass via `Probed` if the probe-failure override ever drifts. The `plan_lock`
test asserts the fallback warning note, so it can only pass for the right reason
(`testing.md`: "Regression tests must fail when the bug is reintroduced").
The arm-agnostic execution sequence (pause -> umount -> forget -> close) is
already covered by `systemd_stop_proceeds_on_running_balance`; per `testing.md`
"keep repro tests focused ... cite that test instead of bundling another phase",
we do not duplicate it.

### Mechanics that make the test self-contained (verified)

- `probe_pool` short-circuits on the first device whose `CryptsetupStatus`
  fails to parse (`parse_cryptsetup_status(...)?` at `cli/src/probe.rs:464`), so
  `braid-bbb` is never probed during `probe_pool`; its single `with_mapper_open`
  mock (seeded by `lock_with_fsid_probe_mocks`) is consumed only by the fallback
  re-scan.
- `MockRunner` checks sequences before single outputs and falls back to the
  single output once a sequence is exhausted (`cli/src/cmd.rs:1541-1562`), so the
  `[err, active]` override on `braid-aaa` cleanly forces a fallback then succeeds
  during fallback classification. The first element -- a non-zero exit (5) with
  non-inactive stderr -- is returned as `Ok` (a non-zero exit is not a
  `CmdError`), so the `?` at `cli/src/probe.rs:460` does not trip; instead
  `parse_cryptsetup_status` fails it as `ParseError::CommandFailed`
  (`cli/src/parse/cryptsetup_status.rs#parse_cryptsetup_status`, locked by
  `cryptsetup_status_errors_on_unexpected_stderr`), surfacing as
  `ProbeError::Parse`. `plan_lock` handles that variant explicitly -- alongside
  `ProbeError::Cmd` -- to drive `Snapshot::ProbeFailed` (`cli/src/lock.rs:874-882`).
- `probe_fsid` reads only `BtrfsFilesystemShow` (already seeded), so it still
  returns the fsid after the per-device probe fails.
- No execute-stage mocks (umount/forget/close) are needed: `plan_lock` plans but
  does not execute.

## Files to modify

- `cli/src/lock.rs` -- add `lock_preflight_pause_decision`, rewire the two arms
  at `:896-927`, and add the new test in `mod tests`.

No other call sites exist (confirmed across `lock.rs`, `recover.rs`, `cmd.rs`).
No docs/ADR/principle change: behavior-preserving, no invariant or user-facing
output change. No new external-tool classifier (the `ProbeFailed` routing is
already behavior-locked by `tests/cli/braid-lock-probe-failed.py`).

## Verification

1. `just test-rust` -- new test passes; all existing lock tests still pass
   (proves the refactor is behavior-preserving).
2. **Confirm the test fails for the right reason** (braid TDD discipline):
   temporarily change the `ProbeFailed` arm to call `require_lock_preflight`
   (the regression the finding describes), re-run `just test-rust`, and confirm:
   - `systemd_stop_probe_failed_fallback_pauses_running_balance` fails (its
     `plan_lock(...).expect(...)` panics because preflight now rejects the
     running balance), and
   - the `Probed`-arm and `User`-arm tests stay green (proves the new test is
     what guards this specific cell).
   Then revert the temporary change.
3. Run the repo's standard format/lint gate (`cargo fmt`, clippy via the
   project's `just` recipe) before committing.
