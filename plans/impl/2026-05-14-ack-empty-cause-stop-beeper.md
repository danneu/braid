# Plan: pin `stop_beeper` on every `causes.is_empty()` positive ack path

## Context

`cmd_ack_impl` and `ack_offline` route a fixed set of positive paths through
`cleanup_alert_files_and_beeper`, which unconditionally calls
`stop_beeper()` at the end. Three of those paths reach cleanup with
`causes.is_empty()` -- the latch contributed nothing parseable to act on,
but cleanup still has to run because some other condition gated entry
(corrupt latch or a bare `smartd-alert` flag).

The current tests for those three paths assert file state and that the
full ack path ran, but never observe the beeper hook. A regression that
gated `stop_beeper()` on `!causes.is_empty()` -- or that early-returned
out of `cleanup_alert_files_and_beeper` before the `stop_beeper()` call --
would silently leave the beeper running on every corrupt-latch ack and on
every smartd-only-mounted ack. The negative case (NotBtrfs) is already
pinned by `cmd_ack_impl_with_foreign_fstype_does_not_invoke_beeper`; the
positive `causes.is_empty()` case is not pinned at all.

The fix is testing-only: extend three existing tests to inject a beeper
counter through `cmd_ack_impl` and assert it fired exactly once.

## Approach

Switch three positive-path tests from `cmd_ack(...)` to
`cmd_ack_impl(..., &beeper)`, where `beeper` is a closure that
increments a `Cell<u32>`. After the existing success assertions, add
`assert_eq!(beeper_calls.get(), 1, "stop_beeper must fire once on <path>")`.

This is the exact pattern already used by
`ack_offline_with_missing_device_cause_marks_missing_acked`
(`cli/src/ack.rs:799-823`). No production-code changes. No new fixtures.

## Tests to modify

All in `cli/src/ack.rs::tests`. For each: declare the counter, swap the
call to `cmd_ack_impl`, add the beeper assertion alongside the existing
success assertions.

1. `cmd_ack_with_mounted_pool_and_corrupt_latch_runs_full_ack_path`
   -- `cli/src/ack.rs:317-343`. Mounted + corrupt latch:
   `causes` empty, `latch_corrupt = true`, cleanup fires.

2. `ack_offline_corrupt_latch_still_clears_files`
   -- `cli/src/ack.rs:974-986`. Offline + corrupt latch:
   `causes` empty, `latch_corrupt = true`, cleanup fires.

3. `cmd_ack_with_mounted_pool_and_smartd_flag_no_latch_runs_full_ack_path`
   -- `cli/src/ack.rs:355-381`. Mounted + bare smartd flag:
   `causes` empty, `smartd_active = true`, cleanup fires.

Assertion message should name the path so a future failure is
self-describing -- e.g. `"stop_beeper must fire once on mounted
corrupt-latch ack"`, `"... offline corrupt-latch ack"`, `"... mounted
smartd-only ack"`.

## Pattern to copy

`cli/src/ack.rs:799-823` (`ack_offline_with_missing_device_cause_marks_missing_acked`):

```rust
let beeper_calls = std::cell::Cell::new(0u32);
let beeper = || beeper_calls.set(beeper_calls.get() + 1);

cmd_ack_impl(&AckPanicRunner, &ack_fs_not_mounted(), &ack_mp(), &paths, &beeper).unwrap();
assert_eq!(
    beeper_calls.get(),
    1,
    "stop_beeper must fire once on offline-ack success"
);
```

The two negative anchors stay untouched: `cmd_ack_impl_with_foreign_fstype_does_not_invoke_beeper`
(`cli/src/ack.rs:762-781`) and the existing offline-missing-device positive
(`cli/src/ack.rs:799-823`).

## Fixtures (already present, no new helpers needed)

From `cli/src/test_fixtures/ack.rs`:

- `AckPanicRunner` -- offline paths.
- `ack_mounted_probe_runner_with_device_stats()` -- mounted full-ack-path paths.
- `ack_fs_btrfs()`, `ack_fs_not_mounted()` -- filesystem surfaces.
- `ack_mp()` -- mount point.
- `isolated_paths()` -- temp state dir + `StatePaths`.

These are already imported in the `tests` module preamble at
`cli/src/ack.rs:266-271`; no new `use` lines required.

## Out of scope

- The other positive-path tests where `causes` is non-empty
  (`*_btrfs_errors_*`, `*_with_smartd_latch_*`, etc.). The first
  regression mode the finding describes ("early-return before
  `stop_beeper`") is already pinned for the `causes` non-empty case by
  the existing missing-device test; the `causes.is_empty()` equivalence
  class is the only positive gap.
- The cleanup-failure tests. Pinning `beeper_calls == 0` after a
  cleanup failure would be useful but adds churn outside the equivalence
  class the finding identified; leave for a follow-up.
- Any production code change. `cleanup_alert_files_and_beeper` and its
  callers are correct today; this plan only pins them with a regression
  guard.

## Verification

1. `just test-rust` -- all three modified tests must pass with the new
   assertion.
2. Optional mutation smoke: temporarily edit
   `cleanup_alert_files_and_beeper` (`cli/src/ack.rs:178-190`) to gate
   the `stop_beeper()` call on a hypothetical `!causes.is_empty()`
   parameter (or to `return Ok(())` before `stop_beeper()`), re-run
   `just test-rust`, and confirm exactly the three modified tests fail
   with the named beeper-assertion message. Revert. This is purely a
   sanity check that the new assertions catch the regression mode the
   finding names.
