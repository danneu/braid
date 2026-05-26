# Plan: pin the BtrfsDeviceErrors ack -> monitor counter-baseline round-trip (Rust)

## Context

`docs/commands/ack.md:5` documents ack's central contract: acking a
`BtrfsDeviceErrors` alert "sets the current device error counts as the new
baseline so the same condition won't re-trigger." Today that contract is only
validated as two isolated halves -- `snapshot_current` writes counters
(`alert.rs:1120` test) and `compute_alert_state` suppresses below a hand-built
baseline (`no_alert_after_ack`, `alert.rs:1047`). The actual `cmd_ack` ->
`cmd_monitor` wiring (snapshot -> persist -> load -> compare -> key) is never
exercised together.

A regression where ack persists a wrong/empty baseline, keys it differently
than monitor reads (`devid.to_string()`), or where the recognized-devid filter
drops the acked row would re-fire the same disk-error alert forever, and no
test would catch it. The only existing end-to-end "does not re-fire" round-trip
(`tests/cli/braid-monitor.py:259`, `tests/module/monitor-lifecycle.py:57`)
covers the **MissingDevice** `missing_acked` flag path, not the counter
baseline. The closest Rust sibling, `stale_mapper_row_with_errors_does_not_latch_or_loop`
(`cli/src/monitor.rs:370`), covers the **negative/unrecognized**-devid case
(monitor -> monitor, no ack).

This was raised as a finding proposing a dm-flakey **repro VM test**. That
vehicle is wrong: the repro lane has no braid dependency
(`tests/repro/kernel-journal-write-error.nix` is "no braid dependency"), its
`braid-alert.service` assertion is redundant (cause-agnostic, already proven in
`monitor-lifecycle.py`), and dm-flakey injection is the fragile, kernel-version
-sensitive part. The three named regressions are pure ack<->monitor state-file
wiring, fully reproducible deterministically in Rust.

**Outcome:** one purely-additive Rust round-trip test, plus one test-only
fixture constructor. Zero production-code changes.

## Approach (chosen): Rust cross-command round-trip, suppression + re-fire

Mirror `stale_mapper_row_with_errors_does_not_latch_or_loop` but for the
positive **recognized-devid + ack-baseline-suppression** path, then add a
re-fire phase. Drive `cmd_monitor` -> `cmd_ack_impl` -> `cmd_monitor`
(-> `cmd_monitor`) over one shared `StatePaths` and one canned runner.

### Why these commands / fixtures compose cleanly (verified)

- Both `cmd_monitor` (`monitor.rs:51`) and `cmd_ack_impl` (`ack.rs:64`) call
  the identical `probe_pool_alerts(runner, fs, mount_point)`, which issues only
  `BtrfsFilesystemShow` + `CryptsetupStatus` (per mapper) on the runner and
  reads mountinfo via `fs` (`probe.rs:302-382`). Each command then issues
  `BtrfsDeviceStatsJson`. `MonitorTestRunner.run` answers exactly those three
  variants, so one runner serves both commands.
- `monitor_fs_btrfs()` mountinfo (`/dev/mapper/braid-vdb`) and `BTRFS_SHOW_2DISK`
  (devid 1 -> braid-vdb, devid 2 -> braid-vdc) align so the probe resolves
  `present_devids = [1,2]`, `recognized_devids = [1,2]`. Use `monitor_mp()`
  (`/mnt/storage`) for all calls.
- `MonitorTestRunner` is stateless for the default-payload path (returns
  `&self.stats_payload` when no one-shot override is queued), so a single
  instance serves all three calls of phases 1-3 with identical counters.
- `cmd_ack_impl`'s `snapshot_current` (`ack.rs:91-98`, `alert.rs:167`) writes
  acked entries for **all** recognized devids (1 and 2), so acked-stats keys are
  "1" and "2" after ack.
- On the second monitor pass, `reconcile_acked_stats` (`alert.rs:264`, called at
  `monitor.rs:105`) keeps keys 1 and 2 (both still-relevant) and only clears
  `missing_acked` (false here) -- it never prunes or mutates the devid-1
  `device_stats` baseline. No interference.

### Step 1 -- test-only fixture (cli/src/test_fixtures/monitor.rs)

Add **only** a general constructor next to `with_override`/`with_stale_mapper_errors`:

```rust
/// Build a runner with a caller-supplied `btrfs device stats` payload so a
/// test can place non-zero counters on a *recognized* devid (the existing
/// stale/healthy constants only zero recognized devids).
pub(crate) fn with_stats_payload(payload: impl Into<String>) -> Self {
    Self { stats_payload: payload.into(), override_op: Mutex::new(None) }
}
```

Do **not** add the payload constants here. The existing stats consts
(`STATS_2DISK_HEALTHY` etc., `monitor.rs:46-70`) are module-private and the
facade (`cli/src/test_fixtures.rs:181`) re-exports only selected helpers, not
the consts -- so consts placed here are invisible to `ack.rs`. Because
`with_stats_payload` takes `impl Into<String>`, the payloads live with the test
instead (Step 2).

### Step 2 -- the test (cli/src/ack.rs, `#[cfg(test)] mod tests`)

Placed in `ack.rs` (not `monitor.rs`) because `cmd_ack_impl` is module-private
and already reachable there via `super::*`; `cmd_monitor`/`MonitorResult` are
`pub`. This needs **no production visibility change**.

Add imports: `use crate::monitor::{cmd_monitor, MonitorResult};` and extend the
`crate::test_fixtures` use with `MonitorTestRunner, monitor_fs_btrfs, monitor_mp`.
(`AlertCause` is already in scope via `super::*`; `load_acked_stats`,
`isolated_paths`, `ack_noop_beeper` are already imported.) No payload-const
import is needed -- the two payloads are defined locally beside the test.

Define the two payloads as `const &str` in `ack.rs::tests`, copying the exact
envelope shape of `STATS_2DISK_HEALTHY` (top-level `{"__header": ..., "device-stats": [...]}`,
all five counter fields per row), mappers matching `BTRFS_SHOW_2DISK` (devid 1
-> braid-vdb, devid 2 -> braid-vdc), changing only devid 1:

```rust
// devid 1 has errors; devid 2 clean. Both are recognized (present in show).
const STATS_DEVID1_ERRORS: &str = r#"{
    "__header": {"version": "1"},
    "device-stats": [
        {"device": "/dev/mapper/braid-vdb", "devid": 1, "write_io_errs": 0, "read_io_errs": 3, "flush_io_errs": 0, "corruption_errs": 1, "generation_errs": 0},
        {"device": "/dev/mapper/braid-vdc", "devid": 2, "write_io_errs": 0, "read_io_errs": 0, "flush_io_errs": 0, "corruption_errs": 0, "generation_errs": 0}
    ]
}"#;
// devid 1 strictly above the acked baseline (read_io_errs 5 > 3) for the re-fire phase.
const STATS_DEVID1_ERRORS_HIGHER: &str = r#"{
    "__header": {"version": "1"},
    "device-stats": [
        {"device": "/dev/mapper/braid-vdb", "devid": 1, "write_io_errs": 0, "read_io_errs": 5, "flush_io_errs": 0, "corruption_errs": 1, "generation_errs": 0},
        {"device": "/dev/mapper/braid-vdc", "devid": 2, "write_io_errs": 0, "read_io_errs": 0, "flush_io_errs": 0, "corruption_errs": 0, "generation_errs": 0}
    ]
}"#;
```

Test body (with the standard 3-section `// Intent / Why it exists / Scenario`
preamble), name e.g. `ack_baseline_suppresses_then_refires_btrfs_device_errors`:

```
let (_dir, paths) = isolated_paths();
let fs = monitor_fs_btrfs();
let mp = monitor_mp();
let runner = MonitorTestRunner::with_stats_payload(STATS_DEVID1_ERRORS);

// Phase 1: monitor latches BtrfsDeviceErrors{devid:1}
let first = cmd_monitor(&runner, &fs, &mp, &paths);
match first { MonitorResult::Alert(s) =>
    assert_eq!(s.causes, vec![AlertCause::BtrfsDeviceErrors { devid: 1 }]),
    other => panic!("expected Alert, got {other:?}") }
assert!(paths.alert_latch_json().exists());

// Phase 2: ack snapshots the live counters as the baseline
cmd_ack_impl(&runner, &fs, &mp, &paths, &ack_noop_beeper).expect("ack ok");
let acked = load_acked_stats(&paths);
let d1 = acked.0.get("1").expect("recognized devid 1 baseline persisted");
assert_eq!(d1.device_stats.read_io_errs, 3);   // right baseline + right key
assert_eq!(d1.device_stats.corruption_errs, 1);
assert!(acked.0.contains_key("2"));             // all recognized devids snapshotted
assert!(!paths.alert_latch_json().exists());    // ack removed the latch

// Phase 3: monitor with the SAME counters must NOT re-fire
let second = cmd_monitor(&runner, &fs, &mp, &paths);
assert_eq!(second, MonitorResult::Ok);
assert!(!paths.alert_latch_json().exists());

// Phase 4 (re-fire): counters above the baseline alert again -> baseline is a
// floor, not a permanent mute for the devid.
let runner_higher = MonitorTestRunner::with_stats_payload(STATS_DEVID1_ERRORS_HIGHER);
let third = cmd_monitor(&runner_higher, &fs, &mp, &paths);
match third { MonitorResult::Alert(s) =>
    assert!(s.causes.contains(&AlertCause::BtrfsDeviceErrors { devid: 1 })),
    other => panic!("expected re-fire Alert, got {other:?}") }
```

The Phase-2 value assertions (`read_io_errs == 3`, key `"1"` present) directly
witness the three named regressions, so a break pinpoints the layer rather than
only flipping Phase 3 red.

## Files to modify

- `cli/src/test_fixtures/monitor.rs` -- add the `with_stats_payload`
  constructor (+ doc comment) only. No new consts here.
- `cli/src/ack.rs` -- in the test module, add the round-trip test, the two
  local `STATS_DEVID1_ERRORS*` payload consts, and the imports
  (`crate::monitor::{cmd_monitor, MonitorResult}` plus the three monitor
  fixtures).

No production source files change.

## Verification

- `just test-rust` -- compiles and runs the `braid-cli` unit tests, including
  the new round-trip test and fixture. (Repo prefers `just test-rust` over
  `cargo test -p braid-cli`.)
- Sanity-check that the test actually pins the wiring: temporarily break one
  regression vector (e.g. make `snapshot_current` skip the erroring devid, or
  key the acked map by something other than `devid.to_string()`), confirm the
  test fails at the expected assertion, then revert.
- No VM tests are required or affected; `just test-vm` is untouched.

## Alternatives considered (rejected)

- **dm-flakey / corruption repro VM test (the original finding's proposal).**
  Rejected: repro lane has no braid dependency; the systemd assertion is
  redundant (cause-agnostic, covered by `monitor-lifecycle.py`); real-tool parse
  and real-devid keying are already covered by golden fixtures /
  `just test-parsers` and the MissingDevice VM round-trip; injection is fragile
  and slow for near-zero marginal coverage.
- **Place the test in `monitor.rs` next to its sibling.** Would require making
  `cmd_ack_impl` `pub(crate)` (a production visibility change + a doc-comment
  justification). `ack.rs` placement avoids any production change.
- **Suppression-only (drop Phase 4).** Pins the three named regressions but not
  the "new errors still alert after ack" direction; Phase 4 is cheap and
  completes the threshold contract through real wiring.
