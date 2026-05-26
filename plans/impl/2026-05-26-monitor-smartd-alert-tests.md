# Plan: pin `cmd_monitor` smartd-alert wiring with Rust unit tests

## Context

`cmd_monitor` (`cli/src/monitor.rs`) reads the smartd flag at step 5
(`monitor.rs:98`, `alert::smartd_alert_active(paths)`) and threads the result
into `compute_alert_state` (`monitor.rs:113-120`), which pushes
`AlertCause::SmartdAlert` (`alert.rs:138-140`). Every *other* live-cause branch
of `cmd_monitor` -- btrfs device errors, missing device, computation error,
corrupt latch, probe failures, offline classification -- has a Rust unit test in
`monitor.rs::tests`. The smartd branch does not: it is exercised only by the
slow VM test `tests/cli/braid-smartd-alert.py`.

Helper-level coverage exists but does not pin the *wiring*:
`smartd_alert_active_requires_regular_file` (`alert.rs:820`) tests the flag
helper in isolation, and `alert_on_smartd` (`alert.rs:1042`) tests
`compute_alert_state(..., true)` in isolation. Neither runs through
`cmd_monitor`, so a refactor that (a) stopped threading `smartd_active` into
`compute_alert_state`, or (b) moved the smartd check ahead of the offline
early-return so an offline pool starts beeping about SMART, would compile and
pass every Rust test, surfacing only in the slow VM lane.

`git log -S 'SmartdAlert' -- cli/src/monitor.rs` is empty: this cause has never
been referenced in `monitor.rs`, so this is a genuine never-covered gap, not a
regression. Outcome: add two structure-insensitive Rust unit tests that bring
the smartd branch to parity with its siblings and pin both regression modes.

## Change

Add two `#[test]` functions to the existing `#[cfg(test)] mod tests` block in
`cli/src/monitor.rs` (after the existing monitor tests, e.g. following
`probe_pool_device_failure_returns_alert_with_latched_computation_error` at
`monitor.rs:820`). Each gets the project-standard three-section `//` preamble
(Intent / Why it exists / Scenario).

All helpers already exist and are already imported into the test module
(`isolated_paths`, `monitor_fs_btrfs`, `monitor_fs_not_mounted`, `monitor_mp`,
`MonitorTestRunner`) or defined locally (`alert_state` at `monitor.rs:181`).
No production code changes. No new fixtures.

### Test 1 (primary): mounted healthy pool + flag -> latched SmartdAlert

Pins regression mode (b): `smartd_active` must be threaded into
`compute_alert_state` and the resulting cause must be merged and persisted.

- Setup: `let (_dir, paths) = isolated_paths();` then
  `std::fs::write(paths.smartd_alert(), b"").unwrap();` (mirrors the
  smartd hook's `touch`, and matches `smartd_alert_active`'s regular-file
  requirement).
- Run: `cmd_monitor(&MonitorTestRunner::with_stale_mapper_stats(), &monitor_fs_btrfs(), &monitor_mp(), &paths)`.
  `with_stale_mapper_stats()` is the established healthy baseline used by the
  sibling `Ok`-asserting tests; its devid-99 stale row is ignored by
  `compute_alert_state`, so with the flag set the only cause is `SmartdAlert`.
- Assert (behavioral, exact):
  - `let state = alert_state(&result);` (panics if not `Alert`)
  - `assert_eq!(state.causes, vec![AlertCause::SmartdAlert]);` -- exactly one
    cause, and it is the smartd cause (no spurious causes from the stale row).
  - Persistence: `let saved = alert::load_alert_latch(&paths).unwrap().unwrap();`
    then `assert_eq!(&saved, state, "saved latch must match returned monitor alert");`
    (mirrors `stats_failure_merges_existing_non_computation_latch_once` at
    `monitor.rs:676-680`; subsumes a bare `alert_latch_json().exists()` check).

Suggested name: `cmd_monitor_latches_smartd_alert_when_mounted`.

### Test 2 (companion): offline pool + flag -> PoolOffline, no latch

Pins regression mode (a): the smartd check must stay *after* the offline
early-return (`monitor.rs:75`), so an unmounted pool with the flag set does not
beep about SMART. The existing `monitor_classifies_unmounted_as_offline`
(`monitor.rs:732`) asserts offline classification but does *not* set the flag,
so it cannot catch a reorder that lets smartd override offline.

- Setup: `let (_dir, paths) = isolated_paths();` then
  `std::fs::write(paths.smartd_alert(), b"").unwrap();`.
- Run: `cmd_monitor(&MonitorTestRunner::with_stale_mapper_stats(), &monitor_fs_not_mounted(), &monitor_mp(), &paths)`.
- Assert:
  - `assert_eq!(result, MonitorResult::PoolOffline);`
  - `assert!(!paths.alert_latch_json().exists(), "offline pool must not latch a smartd alert");`

Suggested name: `cmd_monitor_offline_pool_ignores_smartd_flag`.

## Critical files

- `cli/src/monitor.rs` -- add both tests to the `#[cfg(test)] mod tests` block.
  This is the only file modified.

## Reuse (no new code)

- `crate::test_fixtures::{isolated_paths, monitor_fs_btrfs, monitor_fs_not_mounted, monitor_mp, MonitorTestRunner}`
  -- already imported at `monitor.rs:165-169`.
- `MonitorTestRunner::with_stale_mapper_stats()` (`cli/src/test_fixtures/monitor.rs:108`)
  -- healthy baseline, no override, yields `Ok` with empty causes.
- `alert_state(&MonitorResult)` helper (`monitor.rs:181`) -- extracts the
  `AlertState` or panics.
- `alert::load_alert_latch` (`cli/src/alert.rs:315`) -- read-back for the
  persistence assertion. `AlertState`/`AlertCause` derive `PartialEq`/`Eq`
  (`alert.rs:14-32`), so `==` comparisons hold.
- `StatePaths::smartd_alert()` / `::custom()` (`cli/src/state_paths.rs:31`,`:15`)
  -- flag path resolves inside the isolated temp dir, freely writable.

## Verification

1. Run the new tests: `just test-rust` (the CLI crate is `braid-cli`; the recipe
   handles the package name). During dev, filter with
   `cargo test --lib -p braid-cli smartd` to run just the smartd tests.
2. Confirm both pass.
3. Confirm each test is load-bearing (manual, do not commit the break):
   - Mode (b): temporarily drop `smartd_active` from the `compute_alert_state`
     call in `monitor.rs` -> Test 1 must fail (`Alert` becomes `Ok`/empty causes).
   - Mode (a): temporarily move the `smartd_alert_active` read and an early
     alert ahead of the `if !pool.mounted` return -> Test 2 must fail
     (`PoolOffline` becomes `Alert`).
   Revert after observing the expected failures.
4. No fixture refresh and no VM run required -- this is a pure unit-test
   addition with no parser-critical or systemd-lifecycle blast radius. The VM
   test `tests/cli/braid-smartd-alert.py` remains the end-to-end check and is
   unchanged.
