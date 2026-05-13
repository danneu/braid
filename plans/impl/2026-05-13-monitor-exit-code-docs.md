# Plan: fix monitor.md exit-code documentation

## Context

`manual/commands/monitor.md` documents `braid monitor`'s exit codes
inconsistently with the implementation and with ADR 014:

- The table row for exit 2 says "Monitor error (config unreadable,
  probe failure)" -- but the implementation never returns exit 2 for a
  probe failure. `cmd_monitor` latches every non-`NotBtrfs`
  `ProbeError` (and every parse / btrfs device stats / mountinfo /
  acked-stats baseline / alert latch failure) as an
  `AlertCause::ComputationError`, returns `MonitorResult::Alert`,
  and `main.rs` maps that to exit 1. Exit 2 is emitted exclusively
  by `main.rs` when `config_read` fails before `cmd_monitor` runs.
- The "What triggers an alert (exit 1)" list under the table
  enumerates only `BtrfsDeviceErrors`, `MissingDevice`, and
  `SmartdAlert`. It silently omits `ComputationError`, so a reader
  has no way to learn that probe / parse / btrfs device stats /
  mountinfo / acked-stats baseline / alert latch failures fire the
  beeper.

ADR 014 (`docs/decisions/014-alerts.md:65-78`) is the source of truth
and is correct. The clap `--help` line at `cli/src/main.rs:62` is also
correct. Six tests in `cli/src/monitor.rs` pin the fail-closed
contract for the probe, parse, btrfs device stats, mountinfo, and
acked-stats baseline paths. The user-facing manual drifted; in
addition, the alert-latch-alone fail-closed path -- the `(None,
Some(latch_detail))` branch of `folded_computation_error_detail` at
`cli/src/monitor.rs:21-35` -- has no focused test, even though
`stats_failure_with_corrupt_alert_latch_folds_one_computation_error`
covers the combined-failure case. The manual bullet this plan adds
explicitly promises beeper-triggering behavior for alert-latch
failure alone, so that branch must be pinned alongside the doc fix.

A secondary, less severe inconsistency lives at
`manual/guides/monitoring-and-alerts.md:154`, which schematically
labels exit 2 as just `error`. It's not technically wrong but is
loose; fixing it in the same change keeps the manual self-consistent.

Outcome: operators reading the manual learn the same exit-code
contract that ADR 014 documents and that the implementation enforces.

## Source of truth

- `docs/decisions/014-alerts.md:65-78` -- exit-code contract and
  fail-closed behavior.
- `cli/src/main.rs:62` -- clap doc string (already correct).
- `cli/src/main.rs:668-688` -- only exit values `cmd_monitor` can
  reach: 0 (`PoolOffline` / `Ok`) and 1 (`Alert`); exit 2 lives at
  line 673 on the `config_read` failure path, before `cmd_monitor`.
- `cli/src/monitor.rs:51-69` -- every non-`NotBtrfs` `ProbeError`
  flows to `Err` -> `ComputationError` -> `MonitorResult::Alert`.

## Files to modify

- `manual/commands/monitor.md`
- `manual/guides/monitoring-and-alerts.md`
- `cli/src/monitor.rs` (test addition only -- no production code change)

No production code changes. The fail-closed behavior for probe,
parse, btrfs device stats, mountinfo, and acked-stats baseline is
already pinned by existing tests in `cli/src/monitor.rs`:
`probe_error_returns_alert_with_latched_computation_error`,
`probe_parse_failure_returns_alert_with_latched_computation_error`,
`probe_pool_device_failure_returns_alert_with_latched_computation_error`,
`cmd_monitor_corrupt_acked_stats_latches_computation_error`,
`cmd_monitor_latches_computation_error_on_mountinfo_io_failure`,
`stats_path_failures_return_alert_with_latched_computation_error`.
The combined stats+latch corruption path is pinned by
`stats_failure_with_corrupt_alert_latch_folds_one_computation_error`
(`cli/src/monitor.rs:458`). One new test (Change 3 below) pins the
alert-latch-alone path.

## Change 1: `manual/commands/monitor.md`

### 1a. Rewrite the exit-code table row for code 2 (line 29)

Before:

```
| **2** | Monitor error (config unreadable, probe failure) |
```

After:

```
| **2** | Pre-monitor setup error -- config unreadable |
```

This matches ADR 014's "Reserved for 'could not even attempt to
detect'; never emitted by `cmd_monitor` itself."

### 1b. Add a fourth bullet under "What triggers an alert (exit 1)" (lines 31-35)

Append after the existing three bullets:

```
- **Computation error** -- a probe, parse, btrfs device stats
  call, mountinfo read, acked-stats baseline read, or alert latch
  read failed. Monitor fails closed: it latches a
  `ComputationError` cause so the beeper fires and `braid status`
  shows the detail.
```

Match the existing bullet style (bold lead-in, `--` dash, plain
prose). Use `--` (double hyphen) per the project's CLI / docs style.

## Change 2: `manual/guides/monitoring-and-alerts.md`

### 2a. Tighten the schematic at line 154

Before:

```
    -> braid monitor (exit 0 = ok, 1 = alert, 2 = error)
```

After:

```
    -> braid monitor (exit 0 = ok, 1 = alert, 2 = setup error)
```

One-word change in that schematic; no surrounding prose changes.

## Change 3: `cli/src/monitor.rs` -- pin the alert-latch-alone fail-closed path

### 3a. Add a focused unit test

Add a new `#[test]` inside the existing `mod tests` block (alongside
`stats_failure_with_corrupt_alert_latch_folds_one_computation_error`).
The test exercises the `(None, Some(latch_detail))` branch of
`folded_computation_error_detail` at `cli/src/monitor.rs:21-35` --
healthy probe + healthy stats + corrupt alert latch.

Suggested name: `cmd_monitor_corrupt_alert_latch_latches_computation_error`
(mirrors the existing acked-stats variant).

Test body (sketch -- match the existing test conventions and TDD
preamble; reuse the test fixtures already imported at the top of
`mod tests`):

```rust
// Intent: a corrupt alert latch alone -- with a healthy probe and
//   stats path -- latches a ComputationError and returns
//   MonitorResult::Alert, quarantining the corrupt bytes to the
//   sidecar.
// Why it exists: pins the (None, Some(latch_detail)) branch of
//   folded_computation_error_detail. The existing
//   stats_failure_with_corrupt_alert_latch_folds_one_computation_error
//   only exercises the (Some, Some) branch, so a regression in the
//   latch-alone path could silently pass while the manual still
//   promises a beeper-triggering alert for alert-latch failure
//   alone.
// Scenario: pool is mounted, probe and btrfs device stats succeed
//   cleanly, but alert-latch.json is corrupt (e.g. partial write
//   or hand-edit). cmd_monitor must quarantine the corrupt bytes
//   to alert-latch.json.corrupt, return MonitorResult::Alert with
//   one ComputationError whose detail names the latch quarantine,
//   and write a fresh latch.
#[test]
fn cmd_monitor_corrupt_alert_latch_latches_computation_error() {
    let (_dir, paths) = isolated_paths();
    std::fs::write(paths.alert_latch_json(), b"not json").unwrap();
    let runner = MonitorTestRunner::with_stale_mapper_stats();

    let result = cmd_monitor(&runner, &monitor_fs_btrfs(), &monitor_mp(), &paths);
    let detail = assert_monitor_single_computation_error(&result);
    assert!(
        detail.contains("previous alert latch was unreadable -- quarantined"),
        "detail should name alert latch quarantine, got {detail}"
    );

    let sidecar = std::fs::read(paths.alert_latch_corrupt()).unwrap();
    assert_eq!(
        sidecar,
        b"not json".to_vec(),
        "corrupt alert latch bytes must be preserved"
    );
    assert!(
        paths.alert_latch_json().exists(),
        "fresh alert latch must be written with ComputationError cause"
    );
}
```

Helper / fixture reuse (all already in scope from `mod tests`'s
`use` block at lines 159-164):

- `isolated_paths()` -- temp `StatePaths` -- `cli/src/test_fixtures.rs`.
- `MonitorTestRunner::with_stale_mapper_stats()` -- the healthy-path
  runner used by `stale_mapper_row_no_longer_latches_computation_error`
  at `cli/src/monitor.rs:270-285` (confirms it yields
  `MonitorResult::Ok` when no other failures are injected).
- `monitor_fs_btrfs()`, `monitor_mp()` -- btrfs-mount fixture and
  configured `MountPoint`.
- `assert_monitor_single_computation_error(&result)` -- shared assertion
  that returns the single `ComputationError` detail.

No new helpers required.

## Out of scope

- The mdbook output under `manual/book/` is generated. It will be
  rebuilt the next time the book is built; do not hand-edit it.
- No changes to ADR 014, the clap `--help` doc string, or any Rust
  production code. They are already correct.
- No markdown-fidelity tests for the manual edits -- such tests
  would be fragile, and the underlying behavior is already pinned by
  unit tests in `cli/src/monitor.rs` (with the new test in Change 3
  closing the last gap).

## Verification

1. Re-read `manual/commands/monitor.md` and confirm the exit-code
   table row and trigger list match ADR 014 lines 69-78.
2. Re-read `manual/guides/monitoring-and-alerts.md:154` and confirm
   the schematic now reads `2 = setup error`.
3. `grep -n "probe failure" manual/commands/monitor.md` returns no
   hits (the misleading phrase is gone).
4. `grep -n "Computation error" manual/commands/monitor.md` returns
   the new bullet.
5. Falsifiability check for Change 3's test: after adding the test
   body and confirming it passes, temporarily stub the `(None,
   Some(latch_detail))` arm of `folded_computation_error_detail` to
   return `None` (or break the latch-corrupt thread into the fold),
   re-run the test, and confirm it now fails. Then restore the
   production code and confirm the test passes again. This proves
   the test exercises the branch the plan claims it does.
6. `just test-rust` -- the new test
   `cmd_monitor_corrupt_alert_latch_latches_computation_error`
   passes, and no other Rust tests regress.
7. If mdbook is set up locally, run a manual build to confirm the
   markdown renders cleanly. Otherwise, skip -- the changes are
   plain markdown additions and a one-word edit; CI / book build
   will catch any rendering issue.
