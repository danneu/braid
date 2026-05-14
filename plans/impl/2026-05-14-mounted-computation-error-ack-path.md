# Plan: Pin the mounted `ComputationError`-only ack path

## Context

`cli/src/ack.rs::tests` has dedicated single-cause regression coverage for
the **offline** ack branch on both non-devid causes:

- `ack_offline_smartd_only_latch_does_not_load_acked_stats` (line 1042)
- `ack_offline_computation_error_only_latch_does_not_load_acked_stats` (line 1076)

The **mounted** branch has the equivalent for `SmartdAlert`
(`cmd_ack_mounted_with_smartd_latch_cleans_mid_probe_smartd_flag`, line
546) but **no equivalent for `ComputationError`**. Today the code is
correct for that case -- the gate at `cli/src/ack.rs:64` falls through
on any non-empty `causes`, so a `ComputationError`-only mounted ack
runs the full path (probe `BtrfsDeviceStatsJson`, snapshot a fresh
baseline into `acked-stats.json`, remove the latch, stop the beeper).
But nothing pins that.

The risk this opens: a future "actionable causes" refactor of the
line-64 gate (e.g. filtering to only devid-bearing variants, or
treating `SmartdAlert` as actionable while overlooking
`ComputationError`) would silently no-op `ComputationError`-only
mounted acks. The latch would persist on disk, the beeper would keep
ringing, and the operator's `braid ack` would print "acknowledged
current alerts" or similar without doing anything. Monitor would
re-latch the same `ComputationError` next cycle if the underlying
probe failure is still happening, or leave a stale latch forever if
it has cleared. The existing `SmartdAlert`-mounted test would not
catch this class of regression because `SmartdAlert` has explicit
downstream handling (`latch_had_smartd`, `remove_smartd`) while
`ComputationError` does not.

This is a low-severity, low-cost coverage gap. The fix is one Rust
unit test that mirrors the offline version, using only fixtures that
already exist.

## Critical files

- `cli/src/ack.rs` -- add the new `#[test]` inside `mod tests`.
  - Gate under test: `cli/src/ack.rs:64`
  - Mirror after: `cli/src/ack.rs:1075-1093`
    (`ack_offline_computation_error_only_latch_does_not_load_acked_stats`)
  - Closest mounted siblings to follow for naming/style:
    - `cli/src/ack.rs:317`
      (`cmd_ack_with_mounted_pool_and_corrupt_latch_runs_full_ack_path`)
    - `cli/src/ack.rs:355`
      (`cmd_ack_with_mounted_pool_and_smartd_flag_no_latch_runs_full_ack_path`)

No fixture changes. All needed helpers already exist in
`cli/src/test_fixtures/ack.rs`:

- `isolated_paths` -- temp `StatePaths`
- `ack_write_latch` -- writes `AlertState { causes }` to disk
- `ack_fs_btrfs` -- mounted btrfs `Filesystem` surface
- `ack_mounted_probe_runner_with_device_stats` -- mounted probe runner
  that includes the healthy `BtrfsDeviceStatsJson` mock
- `ack_mp` -- canonical `MountPoint`

## Implementation

Add one test to `cli/src/ack.rs::tests`. Place it directly **after**
`cmd_ack_mounted_with_smartd_latch_cleans_mid_probe_smartd_flag`
(currently ends at line 567) and **before** the `cmd_ack_returns_cleanup_failed_when_remove_smartd_alert_errors_after_baseline_saved` test (line 588). That keeps the
mounted-with-latched-cause tests grouped together.

The test follows the project's `Intent / Why it exists / Scenario`
preamble convention as a contiguous block of `//` line comments
directly above the `#[test]` item, per
[`docs/testing.md`](../../docs/testing.md#preamble-literal--line-comment-form).
This file currently has block-comment preambles for some older tests
(documented drift); new tests use the `//` form.

To pin the beeper side effect on a mounted-success path -- which no
existing test covers -- this test calls `cmd_ack_impl` with an injected
`Cell`-backed hook, mirroring the offline missing-device pattern at
`cli/src/ack.rs:802-817`. The two existing `cmd_ack_impl + Cell-beeper`
tests prove (a) NotBtrfs does not invoke the hook and (b) offline
success invokes it once; nothing yet pins that mounted success
invokes it.

```rust
// Intent: Mounted ack with a parseable latch whose only cause is
//   ComputationError runs the full ack path -- BtrfsDeviceStatsJson is
//   queried, the latch is removed, a fresh acked-stats baseline is
//   written, and the beeper hook fires exactly once.
// Why it exists: The mounted gate at cmd_ack_impl falls through on any
//   non-empty `causes`. A future refactor that narrowed the gate to
//   "actionable" (devid-bearing) causes -- or that special-cased
//   SmartdAlert without doing the same for ComputationError -- would
//   silently no-op a ComputationError-only mounted ack, leaving the
//   latch on disk and the beeper running. The offline equivalent
//   (ack_offline_computation_error_only_latch_does_not_load_acked_stats)
//   does not catch this because the offline branch's gate is
//   structurally different. The beeper-call assertion additionally
//   pins the cleanup hook on the mounted success path -- previously
//   only NotBtrfs (zero) and offline-success (one) call counts were
//   pinned, so a mounted success could regress to skipping the hook
//   without any test failing.
// Scenario: monitor latched a ComputationError on a prior cycle (e.g.
//   a transient probe failure). The pool is now mounted and healthy;
//   the operator runs `braid ack`. The latch must be cleared, a fresh
//   baseline persisted, and the beeper silenced.
#[test]
fn cmd_ack_with_mounted_pool_and_computation_error_only_latch_runs_full_ack_path() {
    let (_dir, paths) = isolated_paths();
    ack_write_latch(
        &paths,
        vec![AlertCause::ComputationError {
            detail: "test".to_owned(),
        }],
    );
    let runner = ack_mounted_probe_runner_with_device_stats();
    let beeper_calls = std::cell::Cell::new(0u32);
    let beeper = || beeper_calls.set(beeper_calls.get() + 1);

    let result = cmd_ack_impl(&runner, &ack_fs_btrfs(), &ack_mp(), &paths, &beeper);

    assert!(
        result.is_ok(),
        "computation-error-only ack should succeed, got {result:?}"
    );
    assert!(
        runner
            .requests()
            .iter()
            .any(|r| matches!(r, CmdRequest::BtrfsDeviceStatsJson { .. })),
        "computation-error-only ack must run the full ack path"
    );
    assert!(
        !paths.alert_latch_json().exists(),
        "latch must be removed"
    );
    assert!(
        paths.acked_stats_json().exists(),
        "mounted ack must persist a fresh baseline"
    );
    assert_eq!(
        beeper_calls.get(),
        1,
        "stop_beeper must fire once on mounted-ack success"
    );
}
```

### Why these five assertions

Each assertion pins one observable in the regression-risk path; none
depend on internal layout.

1. `result.is_ok()` -- a regression that errored on this case (e.g.
   panicked while filtering causes) would be caught.
2. `runner.requests()` contains `BtrfsDeviceStatsJson` -- the load-
   bearing assertion against the line-64 gate. A regression that
   early-returned on non-actionable causes would skip this command.
   Same shape as `cmd_ack_with_mounted_pool_and_smartd_flag_no_latch_runs_full_ack_path` (line 366-372).
3. `!paths.alert_latch_json().exists()` -- pins that
   `cleanup_alert_files_and_beeper` ran. A regression that early-
   returned without cleanup would leave the latch in place.
4. `paths.acked_stats_json().exists()` -- pins that `save_acked_stats`
   ran. A regression that skipped baselining for non-actionable
   causes would not produce the file.
5. `beeper_calls.get() == 1` -- pins that the cleanup hook reaches
   `stop_beeper()` on mounted success. Previously only NotBtrfs
   (zero, line 763-781) and offline missing-device success (one, line
   798-823) were pinned; a regression that skipped or short-circuited
   the hook on a mounted success path would otherwise pass with the
   first four assertions alone, since the production `stop_beeper` is
   a `cfg(test)` no-op when reached via the `cmd_ack` wrapper.

### What this test does NOT need

- No new fixture functions.
- No changes to `cli/src/test_fixtures/ack.rs` (all imports already
  brought into scope at lines 266-271).
- No changes to production code in `cli/src/ack.rs`.
- No additional documentation.

## Verification

End-to-end check that the test was added correctly and passes against
current behavior:

```sh
just test-rust
```

Specifically the new test will run inside the `braid-cli` package's
`ack::tests` module. To exercise just it:

```sh
cargo test -p braid-cli --lib ack::tests::cmd_ack_with_mounted_pool_and_computation_error_only_latch_runs_full_ack_path
```

To confirm the test catches the hypothesized regression (manual
sanity check, do **not** commit): temporarily change the gate at
`cli/src/ack.rs:64` to the regressed form and re-run the test:

```rust
// Hypothetical regression for verification only -- revert before commit.
let actionable = causes
    .iter()
    .any(|c| matches!(c, AlertCause::MissingDevice { .. }
                       | AlertCause::BtrfsDeviceErrors { .. }
                       | AlertCause::SmartdAlert));
if !actionable && !smartd_active && !latch_corrupt {
    println!("no active alerts");
    return Ok(());
}
```

The new test should fail (the `BtrfsDeviceStatsJson` assertion and
both file-existence assertions). Existing mounted tests should still
pass. Revert the gate change and confirm the new test passes again.

## Out of scope

- No production code changes.
- No refactor of the gate at `cli/src/ack.rs:64`. The current
  implementation is correct; this plan only adds coverage.
- No symmetric audit for other under-covered cause/branch combos --
  the offline+mounted matrix for the four `AlertCause` variants is
  otherwise covered (see file map in Context).
