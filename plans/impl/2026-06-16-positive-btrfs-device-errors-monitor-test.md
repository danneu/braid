# Plan: pin the positive BtrfsDeviceErrors detection path in `monitor.rs`

## Context

`braid monitor`'s most fundamental detection path is: a present pool member
logging non-zero btrfs error counters -> `BtrfsDeviceErrors { devid }` cause ->
active alert latch -> exit 1 -> beep. `monitor.rs`'s own `mod tests` pins every
*other* `cmd_monitor` alert outcome -- `ComputationError` (many variants),
`MissingDevice` carry-forward (`healthy_cycle_carries_forward_existing_non_computation_latch`),
`SmartdAlert` wiring (`cmd_monitor_latches_smartd_alert_when_mounted`), and the
two *negative* device-error cases (`stale_mapper_row_no_longer_latches_computation_error`,
`stale_mapper_row_with_errors_does_not_latch_or_loop`, both asserting `Ok` /
no-latch) -- but never the *positive* device-error outcome.

That positive path is exercised today only as **Phase-1 setup** of
`cli/src/ack.rs`'s `ack_baseline_suppresses_then_refires_btrfs_device_errors`,
whose real subject is ack baseline suppress/re-fire. Two gaps follow:

- **Fragility.** If that ack test is ever refactored to seed its starting latch
  directly (e.g. via `save_alert_latch`) instead of routing through
  `cmd_monitor`, `monitor`'s positive detection path loses *all* coverage, with
  no monitor-local test to notice. A regression that inverted the
  recognized-devid filter (dropping recognized rows) would also pass *both*
  monitor negatives, since both assert `Ok`/no-latch -- only a positive
  assertion catches it.
- **An unmade round-trip assertion.** The ack test's Phase 1 only asserts
  `alert_latch_json().exists()`; it never reloads the latch and compares it to
  the returned state for a `BtrfsDeviceErrors` cause. `monitor.rs`'s convention
  (e.g. `healthy_cycle_...`, `cmd_monitor_latches_smartd_alert_when_mounted`) is
  to reload-and-compare. That round-trip is uncovered for this cause anywhere.

This is the salvageable core of a review finding whose *headline* ("no test
proves this end-to-end", "a regression would pass every existing test") is
overstated -- the ack test does cover it incidentally. The work is a focused,
Low-severity symmetry/robustness test, not the Medium coverage hole the finding
described.

## Change

Add **one** unit test to `cli/src/monitor.rs`'s `mod tests`. No changes to
`ack.rs` or `test_fixtures` (see "Rejected alternative" for why the const is
local, not shared).

**Location:** immediately after `stale_mapper_row_with_errors_does_not_latch_or_loop`
(`cli/src/monitor.rs:428-454`), so the positive device-error case sits beside
its negative counterparts.

**Reused infrastructure (all already in scope via `use super::*` + the existing
test imports -- no new `use` lines):**
- `MonitorTestRunner::with_stats_payload(payload)` -- `cli/src/test_fixtures/monitor.rs:134`.
  Its runner already serves `BTRFS_SHOW_2DISK` (recognized devids 1 and 2 ->
  `/dev/mapper/braid-vdb`, `/dev/mapper/braid-vdc`) and the matching
  `CryptsetupStatus`, so devids 1 and 2 are present and recognized.
- `alert_state(&result)` helper -- `cli/src/monitor.rs:177`.
- `alert::load_alert_latch(&paths).unwrap().unwrap()` reload idiom -- as used at
  `cli/src/monitor.rs:786` and `:953`.
- `isolated_paths`, `monitor_fs_btrfs`, `monitor_mp`, `Devid`, `AlertCause` --
  already imported by the test module.

**Test shape** (`//` Intent / Why it exists / Scenario preamble per AGENTS.md,
matching the most recent monitor tests):

```rust
// Intent: cmd_monitor latches exactly BtrfsDeviceErrors { devid } for a
//   recognized, present pool member whose btrfs device stats row carries
//   non-zero error counters, and the saved alert latch reloads to that same
//   AlertState.
// Why it exists: this is monitor's most fundamental detection path -- a real
//   disk logging read/corruption errors -> beep. monitor.rs's own suite pins
//   only the NEGATIVE device-error cases
//   (stale_mapper_row_no_longer_latches_computation_error,
//   stale_mapper_row_with_errors_does_not_latch_or_loop, both asserting
//   Ok/no-latch) plus the other cause families. The positive path is exercised
//   only incidentally as Phase-1 setup of ack.rs's
//   ack_baseline_suppresses_then_refires_btrfs_device_errors; a refactor of that
//   ack test that seeds its latch directly would erase monitor's only positive
//   coverage, and a regression that inverted the recognized-devid filter would
//   pass both monitor negatives. The latch reload-compare also pins a round-trip
//   the ack test never makes (it only asserts the latch file exists).
// Scenario: a mounted, recognized 2-disk pool; btrfs device stats reports
//   non-zero read_io_errs/corruption_errs on devid 1 (present member
//   /dev/mapper/braid-vdb) and clean counters on devid 2. monitor must latch
//   exactly BtrfsDeviceErrors { devid: 1 } and persist it so alert-latch.json
//   reloads to the returned state.
#[test]
fn cmd_monitor_latches_btrfs_device_errors_for_recognized_devid() {
    // Non-zero counters on recognized devid 1 (/dev/mapper/braid-vdb in
    // BTRFS_SHOW_2DISK); devid 2 clean. The healthy/stale fixtures only zero
    // recognized devids, so supply the payload via with_stats_payload.
    const STATS_DEVID1_ERRORS: &str = r#"{
    "__header": {"version": "1"},
    "device-stats": [
        {"device": "/dev/mapper/braid-vdb", "devid": 1, "write_io_errs": 0, "read_io_errs": 3, "flush_io_errs": 0, "corruption_errs": 1, "generation_errs": 0},
        {"device": "/dev/mapper/braid-vdc", "devid": 2, "write_io_errs": 0, "read_io_errs": 0, "flush_io_errs": 0, "corruption_errs": 0, "generation_errs": 0}
    ]
}"#;

    let (_dir, paths) = isolated_paths();
    let runner = MonitorTestRunner::with_stats_payload(STATS_DEVID1_ERRORS);

    let result = cmd_monitor(&runner, &monitor_fs_btrfs(), &monitor_mp(), &paths);

    // Exactly one cause, the right devid: proves the clean devid-2 row
    // contributed nothing and no spurious ComputationError was folded in.
    let state = alert_state(&result);
    assert_eq!(
        state.causes,
        vec![AlertCause::BtrfsDeviceErrors {
            devid: Devid::new(1)
        }],
        "recognized devid 1 with non-zero counters must latch exactly its btrfs error"
    );

    // The saved latch must round-trip to the same AlertState -- ack.rs's Phase 1
    // only asserts the file exists, never reloads it for a BtrfsDeviceErrors cause.
    let saved = alert::load_alert_latch(&paths).unwrap().unwrap();
    assert_eq!(&saved, state, "saved latch must match returned monitor alert");
}
```

The single `assert_eq!` on `state.causes` subsumes "carries *exactly*
`BtrfsDeviceErrors { devid }`": it pins one cause, the correct devid, no
`ComputationError`, and that the clean devid-2 row stays out. Re-fire /
suppression semantics stay out of scope -- that is the ack test's job.

## Rejected alternative: promoting a shared `STATS_DEVID1_ERRORS` fixture

The original finding floated promoting the payload into `test_fixtures/monitor.rs`
as a `pub(crate) const` shared by both `ack.rs` and the new monitor test.
Rejected:

- **It re-couples the two test modules.** The point of this change is to make
  `monitor`'s detection coverage self-sufficient and independent of the ack
  test; a shared payload const reintroduces a coupling whose shape either test
  could later need to change.
- **No precedent.** Every `STATS_*` const in `test_fixtures/monitor.rs` is
  private; the module exposes runners/helpers, and custom payloads are passed
  via `with_stats_payload`. The ack test defines its payload locally. A shared
  `pub(crate) const` string fixture would be a novel pattern for a ~6-line JSON
  string.
- **Monitor needs only the base payload**, not ack's `STATS_DEVID1_ERRORS_HIGHER`
  re-fire variant, so there is no clean shared unit to extract.

A local `const` in the new test matches the one existing `with_stats_payload`
caller exactly and keeps each test self-documenting.

## Files modified

- `cli/src/monitor.rs` -- add the one test described above. (Only file touched.)

## Verification

1. **Targeted run -- the new test passes** (it pins existing correct behavior,
   so it should be green immediately):
   ```
   cd cli && cargo test cmd_monitor_latches_btrfs_device_errors_for_recognized_devid -- --nocapture
   ```
2. **Confirm it has teeth** (optional, manual): temporarily assert
   `result == MonitorResult::Ok`, or break the assertion to
   `Devid::new(2)`, and confirm the test fails -- proving the positive
   assertion would catch a recognized-row filter inversion or a wrong-devid
   keying. Revert.
3. **Full Rust suite unaffected:**
   ```
   just test-rust
   ```
   (No fixture/parser changes, so `just capture-all-fixtures` / `just test-parsers`
   are not implicated.)
