# Plan: pin no-quarantine in ack via a cleanup-interrupted test

## Context

A reviewer flagged the asymmetry between `cmd_monitor` (calls
`load_alert_latch_or_quarantine`, moves a corrupt `alert-latch.json` to
the `.corrupt` sidecar) and `cmd_ack` (calls plain `load_alert_latch`,
then cleanup deletes the latch). The asymmetry is intentional: monitor
rewrites the latch (so it must preserve bytes before clobbering them),
ack deletes the latch (so quarantine would be a different way of
deleting). The design is documented at:

- `docs/decisions/014-alerts.md:109` -- "Mounted ack and genuinely
  unmounted ack clean up both `alert-latch.json` and the `.corrupt`
  sidecar."
- `manual/commands/ack.md:41` -- step 6 "Removes the corrupt-latch
  sidecar ... if present."
- Commit `f878bf8` ("fix(cli): distinguish absent vs corrupt alert
  latch") and the implementation plan at
  `plans/impl/2026-04-27-alert-latch-corrupt-recovery.md:367-371`.

The two existing success-path corrupt-latch tests
(`cmd_ack_with_mounted_pool_and_corrupt_latch_runs_full_ack_path` at
`cli/src/ack.rs:347` and `ack_offline_corrupt_latch_still_clears_files`
at `cli/src/ack.rs:1524`) cannot pin the no-quarantine invariant on
their own: `cleanup_alert_files_and_beeper` at `cli/src/ack.rs:201`
always calls `remove_alert_latch_corrupt` on the success path, so the
sidecar's absence at test end is true whether or not quarantine ran
mid-execution. Any `!paths.alert_latch_corrupt().exists()` assertion
added to those success tests is a tautology -- a future regression that
swapped `load_alert_latch` for `load_alert_latch_or_quarantine` at
`cli/src/ack.rs:35` would create the sidecar at line 35 and silently
have it deleted by cleanup at line 212, with the test still passing.

Pivot: pin the invariant at a state where cleanup cannot erase the
evidence. Poisoning `alert-cleanup-pending` (placing a directory at
that path) makes `mark_alert_cleanup_pending` fail at
`cleanup_alert_files_and_beeper`'s second step, before any destructive
removal runs. Any sidecar created by a hypothetical quarantine survives
that early return and is visible to the test. The pattern already
exists for valid latches in
`cmd_ack_mounted_retry_after_poisoned_sentinel_completes_recovery` at
`cli/src/ack.rs:1055`; the new test mirrors its setup with a corrupt
latch.

Outcome: a single behavioral regression test that fails if a future
edit makes ack quarantine corrupt latch bytes -- closes the gap the
finding identified.

## Change

Single file: `cli/src/ack.rs`. One new test inside the `#[cfg(test)]
mod tests` block, placed near the existing poisoned-sentinel test at
line 1055 for locality.

```rust
// Intent: ack does not quarantine a corrupt alert-latch.json. The
//   no-quarantine invariant is the monitor/ack asymmetry: monitor
//   moves corrupt bytes to alert-latch.json.corrupt because it rewrites
//   the latch, ack deletes the latch outright in cleanup.
// Why it exists: a naive symmetry edit could swap load_alert_latch for
//   load_alert_latch_or_quarantine at cmd_ack_impl. On the success
//   path the sidecar would be created by quarantine and then deleted
//   by remove_alert_latch_corrupt during cleanup, so any
//   sidecar-absence assertion at the end of a successful ack proves
//   nothing. Forcing cleanup to fail at mark_alert_cleanup_pending
//   stops execution before any destructive removal runs, so any
//   sidecar created by quarantine persists and is visible to the test.
// Scenario: corrupt latch on disk; the alert-cleanup-pending path is a
//   directory (manual tampering or leftover from a previous bug). ack
//   must return CleanupFailed, preserve the original corrupt latch
//   bytes verbatim, and not create a .corrupt sidecar.
#[test]
fn cmd_ack_mounted_corrupt_latch_does_not_quarantine_when_cleanup_fails() {
    let (_dir, paths) = isolated_paths();
    let original_bytes: &[u8] = b"not json";
    std::fs::write(paths.alert_latch_json(), original_bytes).unwrap();
    std::fs::create_dir(paths.alert_cleanup_pending()).unwrap();

    let runner = ack_mounted_probe_runner_with_device_stats();
    let beeper_calls = std::cell::Cell::new(0u32);
    let beeper = || beeper_calls.set(beeper_calls.get() + 1);

    let err = cmd_ack_impl(&runner, &ack_fs_btrfs(), &ack_mp(), &paths, &beeper)
        .expect_err("marker creation must fail on the poisoned sentinel path");
    assert!(
        matches!(err, AckError::CleanupFailed(_)),
        "expected AckError::CleanupFailed, got {err:?}"
    );

    assert_eq!(
        std::fs::read(paths.alert_latch_json()).unwrap(),
        original_bytes,
        "corrupt latch bytes must remain untouched because no destructive removal ran"
    );
    assert!(
        !paths.alert_latch_corrupt().exists(),
        "ack must not quarantine -- monitor is the only path that creates the sidecar"
    );
}
```

The test relies on existing helpers already imported in
`cli/src/ack.rs:294-301`'s `mod tests`: `isolated_paths`,
`ack_mounted_probe_runner_with_device_stats`, `ack_fs_btrfs`, `ack_mp`,
and the `cmd_ack_impl` function itself. No new fixtures or helpers.

## Non-changes

- No production code change. `cmd_ack_impl` keeps calling
  `load_alert_latch`; `cleanup_alert_files_and_beeper` is untouched.
- No edits to the existing success-path corrupt-latch tests
  (`cmd_ack_with_mounted_pool_and_corrupt_latch_runs_full_ack_path` at
  `cli/src/ack.rs:347` and `ack_offline_corrupt_latch_still_clears_files`
  at `cli/src/ack.rs:1524`). The originally proposed
  `!alert_latch_corrupt().exists()` assertions in those tests would be
  tautologies (cleanup always deletes the sidecar on success), so they
  add no regression coverage and would create a false sense of
  protection. The new test carries the documentation signal via its
  preamble and `assert!` messages.
- No doc edits. ADR 014, the manual, and the corrupt-latch
  implementation plan all already state the rule.

## Verification

```
just test-rust
```

The new test lives in `cli/src/ack.rs`'s `tests` module; `just
test-rust` runs the full `cargo test` suite for the CLI crate
(`braid-cli`). The asserted conditions are already true in production
today, so the test must pass unchanged.

Sanity check that the test is at the failure layer (not a tautology):
temporarily replace `alert::load_alert_latch(paths)` with
`alert::load_alert_latch_or_quarantine(paths)` at `cli/src/ack.rs:35`
(adjusting the `match` arms to match the helper's `(Option, Option)`
return shape -- simulating the regression). Re-run `just test-rust`;
the new test must fail on both the latch-bytes-preserved assertion
(file was moved by quarantine, so `std::fs::read` returns NotFound)
and the no-sidecar assertion. Revert the production change.
