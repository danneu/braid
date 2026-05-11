# Plan: monitor's acked-stats read must fail-closed on corruption

## Context

`cmd_monitor` is a mutation path (it writes the alert latch and reconciles
`acked-stats.json`), but it still reads `acked-stats.json` through
`alert::load_acked_stats` -- the lossy loader that swallows both `NotFound`
and parse errors into `AckedStats::default()`. When the file is corrupt
(manual edit, non-braid writer, or filesystem damage):

1. `load_acked_stats` returns an empty `AckedStats`.
2. `reconcile_acked_stats(&mut empty, ...)` walks an empty map, so
   `ack_changed` stays false and `save_acked_stats` is never called. The
   corrupt file on disk is preserved.
3. `compute_alert_state` runs against the empty baseline:
   - Every nonzero counter in `btrfs device stats` trips
     `BtrfsDeviceErrors { devid }` (because `has_new_errors` compares
     against the zero baseline).
   - Every alert-missing devid trips `MissingDevice { devid }` (because
     `missing_acked` defaults to false).
4. `merge_into_latch` writes the new causes; the systemd wrapper sees
   exit 1 and starts the beeper. Operator hears a beep with no truthful
   indication of why; `braid status` shows the false causes, not a
   `ComputationError`.

This violates ADR 014's fail-closed contract for indeterminate state
(`docs/decisions/014-alerts.md:74`): "any failure inside `cmd_monitor`
that leaves pool state indeterminate latches a `ComputationError` cause."
An unreadable ack baseline is the textbook indeterminate-state case --
we cannot know which historical alerts were acked.

`monitor` is the **only** production mutation path still on the lossy
loader. Sibling paths already use `load_acked_stats_fallible`:

- `ack_offline` (`cli/src/ack.rs:144`) -- propagates corruption as
  `AckError::Io`.
- `drop_ghost_acked_for_devids` (`cli/src/alert.rs:222`) -- propagates
  corruption as `std::io::Error`.

ADR 014:124 explicitly enumerates those two as fallible-loader users
"matching the policy in `drop_ghost_acked_for_devids`"; `monitor` was
simply missed when the fallible loader landed (`92ad988` added it for
the add-time correctness boundary, `2ababfd` extended it to offline ack).

## Approach

Switch `cmd_monitor`'s `acked-stats.json` read to the fallible loader and
route failures through the existing `latch_computation_error` helper.
Pin the regression with a behavioral unit test.

### 1. Production code change

**File:** `cli/src/monitor.rs`

At line 96, replace the lossy load with the fallible loader and a
ComputationError branch on Err. The detail string follows the ADR 014
"unreadable -- {e}" convention already used by `resolve_alert_state` in
`status.rs:466`.

```rust
// 3. Load acked stats. Fail closed if the file is unreadable or
// unparseable: an empty fallback would silently re-fire every acked
// cause as a BtrfsDeviceErrors / MissingDevice cause against a zero
// baseline.
let mut acked = match alert::load_acked_stats_fallible(paths) {
    Ok(a) => a,
    Err(e) => return latch_computation_error(
        format!("acked-stats unreadable -- {e}"),
        paths,
    ),
};
```

At line 5-7, drop `load_acked_stats` from the named import list (the
remaining production code uses only `load_acked_stats_fallible` via the
`alert::` prefix; tests at `monitor.rs:218` already use
`alert::load_acked_stats(&paths)` via the `self` import):

```rust
use crate::alert::{
    self, AlertCause, compute_alert_state, merge_into_latch, save_acked_stats,
};
```

### 2. New unit test

**File:** `cli/src/monitor.rs` (in the existing `#[cfg(test)] mod tests`)

Add `cmd_monitor_corrupt_acked_stats_latches_computation_error`, modeled
on `ack_offline_corrupt_acked_stats_propagates_io_error_when_missing_cause`
at `ack.rs:1004-1023`. The test must pin three invariants:

1. The result is `MonitorResult::Alert(state)` with exactly one cause,
   `ComputationError`, whose detail names "acked-stats" (assert via the
   existing `assert_monitor_single_computation_error` helper at
   `test_fixtures/monitor.rs:265`).
2. The latch was written (`paths.alert_latch_json().exists()`).
3. The corrupt bytes on disk are byte-identical to the input -- monitor
   must NOT silently rewrite the corrupt file (mirrors the byte-identity
   assertion at `ack.rs:1018-1022`).

Use `MonitorTestRunner::with_stale_mapper_stats()` + `monitor_fs_btrfs()`
+ `monitor_mp()` for the healthy probe/stats surface, and write
`b"not json"` to `paths.acked_stats_json()` before invoking
`cmd_monitor`. The runner emits zero counters; the regression mode
(without the fix) would be `MonitorResult::Ok`, which the
`assert_monitor_single_computation_error` helper catches loudly.

Test preamble follows the literal `//` line-comment form from
`docs/testing.md:11-22` (contiguous `//` lines directly above the
test item, **not** the legacy `/* ... */` block-comment form some
existing tests in this file still use):

```rust
// Intent: cmd_monitor returns MonitorResult::Alert with exactly one
//   ComputationError cause whose detail names "acked-stats" when
//   acked-stats.json is unreadable / unparseable, and the corrupt
//   bytes on disk are preserved byte-identical.
// Why it exists: pins use of load_acked_stats_fallible (not the
//   lossy load_acked_stats) on monitor's mutation path. Without it,
//   cmd_monitor would treat a corrupt acked-stats.json as an empty
//   baseline and silently return MonitorResult::Ok against an
//   otherwise-healthy pool -- a fail-open hole in the indeterminate-
//   state contract pinned by ADR 014:74. The byte-identity assertion
//   also pins that monitor must not silently rewrite corrupt files
//   (mirrors ack.rs:1018-1022).
// Scenario: acked-stats.json was hand-edited to invalid JSON; the
//   pool is mounted and healthy, btrfs device stats reports zero
//   counters on both members. cmd_monitor must surface the
//   corruption as a single ComputationError cause and leave the
//   corrupt file on disk.
#[test]
fn cmd_monitor_corrupt_acked_stats_latches_computation_error() { ... }
```

### 3. ADR 014 docs update

**File:** `docs/decisions/014-alerts.md`

The "Monitor reconcile" sub-bullet (line ~136 under the "Acked-stats
hygiene" section) describes `cmd_monitor`'s pruning role but does not
mention what happens when the file is unreadable. Append one sentence
clarifying that `monitor` uses `load_acked_stats_fallible` so corruption
surfaces as `ComputationError`, matching the policy at the offline-ack
and add-time call sites already enumerated in line 124.

Suggested addition (one sentence appended to bullet 3):

> The read itself uses `load_acked_stats_fallible` so a corrupt or
> unreadable `acked-stats.json` latches `ComputationError` instead of
> silently re-firing acked causes against an empty baseline (same policy
> as offline ack and `drop_ghost_acked_for_devids`).

### 4. Doc-comment guard on the lossy loader

**File:** `cli/src/alert.rs`

After this fix, no production mutation path uses `load_acked_stats`
(remaining callers are all in `#[cfg(test)] mod tests` blocks that
deserialize on-disk state to assert reload behavior). To prevent a
future regression that re-introduces the lossy loader on a mutation
path, add a `///` doc comment on the function at `alert.rs:63` naming
the constraint:

```rust
/// Lossy loader: swallows both `NotFound` and parse errors into
/// `AckedStats::default()`. Use only for test reload assertions or
/// strictly read-only inspection. Production mutation paths must use
/// `load_acked_stats_fallible` so corruption surfaces as
/// `ComputationError` per ADR 014 (`docs/decisions/014-alerts.md:74`).
pub fn load_acked_stats(paths: &StatePaths) -> AckedStats {
```

Same constraint applies to `load_acked_stats_at` at line 67 -- a single
sentence reusing the policy from the wrapper is enough.

## Files to modify

- `cli/src/monitor.rs` -- production code change at line 96, import
  cleanup at lines 5-7, new test in the existing tests module.
- `cli/src/alert.rs` -- add doc comments on `load_acked_stats` (line 63)
  and `load_acked_stats_at` (line 67).
- `docs/decisions/014-alerts.md` -- append one sentence to the "Monitor
  reconcile" bullet near line 136.

## Reused existing utilities

- `alert::load_acked_stats_fallible` (`cli/src/alert.rs:198-208`) -- the
  fallible loader already used by `ack_offline` and
  `drop_ghost_acked_for_devids`.
- `latch_computation_error` (`cli/src/monitor.rs:24-44`) -- already
  handles existing latch quarantine and detail folding; the helper's
  return type (`MonitorResult::Alert`) is exactly what we want.
- `assert_monitor_single_computation_error` (`cli/src/test_fixtures/monitor.rs:265-282`)
  -- existing helper asserting exactly-one-cause + ComputationError +
  returns the detail substring.
- `MonitorTestRunner::with_stale_mapper_stats()`, `monitor_fs_btrfs()`,
  `monitor_mp()`, `isolated_paths()` -- existing healthy-probe fixtures
  used by every other monitor unit test.

## Verification

1. `just test-rust` -- runs the new
   `cmd_monitor_corrupt_acked_stats_latches_computation_error` test and
   confirms the existing monitor tests (notably
   `cmd_monitor_reconciles_acked_stats_across_pool_axes`,
   `stale_mapper_row_no_longer_latches_computation_error`, and all
   `probe_*_returns_alert_with_latched_computation_error` tests) still
   pass. The NotFound path is preserved (fallible loader returns
   `Ok(AckedStats::default())` on `ErrorKind::NotFound`), so
   no-acked-file tests do not regress.

2. Inspect that the test fails without the production change by
   temporarily reverting `monitor.rs:96` to `load_acked_stats(paths)`
   and re-running -- expect a panic from
   `assert_monitor_single_computation_error` because the regressed code
   returns `MonitorResult::Ok` (corrupt file -> empty acked baseline ->
   zero counters means no causes -> Ok).

3. `cargo clippy --all-targets` -- catches unused-import warnings if the
   `load_acked_stats` import cleanup is incomplete.

No VM test addition is required: the bug is internal to `cmd_monitor`'s
control flow and is fully exercised by the unit test. The existing
`braid-monitor` VM tests cover the end-to-end smartd / offline-ack
interactions and remain unaffected.
