# Refactor Monitor ComputationError Latching

## Summary

Collapse `cmd_monitor` to one alert-latch merge/save path. Preserve behavior:
offline paths still return without touching the latch, indeterminate failures
still latch one `ComputationError`, and corrupt alert-latch details still get
folded into that single slot.

## Key Changes

- Remove `latch_computation_error`.
- In `cmd_monitor`, first classify the monitor pass with a local closure or
  block returning `Result<Option<Vec<AlertCause>>, String>`:
  - `Ok(None)` means `NotBtrfs` or unmounted pool; return
    `MonitorResult::PoolOffline` before loading/quarantining the latch.
  - `Ok(Some(causes))` means normal mounted live causes.
  - `Err(detail)` means a fail-closed probe failure, btrfs stats
    command/parse failure, or `load_acked_stats_fallible` read/parse failure.
- Preserve the current exhaustive `ProbeError` classification gate with no
  wildcard or catch-all arm: `NotBtrfs` maps to `Ok(None)`, while `Cmd`,
  `Parse`, `PoolDevice`, `UnsupportedLuksVersion`, `MapperConflict`, and
  `MountInfo` each map to `Err(e.to_string())`. A future `ProbeError` variant
  must fail compilation until monitor explicitly classifies it.
- Keep reconcile save failure warning-only: a failed
  `save_acked_stats(&acked, paths)` after `reconcile_acked_stats` must still
  print `Warning: failed to update acked stats: {e}` and continue.
- After the offline gate, call `alert::load_alert_latch_or_quarantine(paths)`
  exactly once.
- If the classified result was `Err(detail)`, print
  `eprintln!("error: {detail}")` exactly once before latch load/fold/save.
- Add one private pure helper in `monitor.rs`, with a `///` comment, to fold
  `failure_detail: Option<String>` and `latch_corrupt_detail: Option<String>`
  into at most one `ComputationError` detail:
  - failure only: original failure detail
  - latch only: `previous alert latch was unreadable -- quarantined; ...`
  - both:
    `{failure}; additionally, previous alert latch was unreadable -- quarantined; ...`
  - neither: no computation error
- Build `live_causes` once, insert the folded `ComputationError` at index 0
  when present, then use one `merge_into_latch`, one active
  `save_alert_latch`, and one final `MonitorResult::Alert(merged)` branch.

## Interfaces

- No public API changes.
- No CLI output, JSON schema, latch format, ack semantics, or ADR behavior
  changes.
- `status` and `ack` stay out of scope because their corrupt-latch behavior is
  intentionally different.

## Test Plan

- Add a monitor unit test for a stats failure plus corrupt `alert-latch.json`;
  assert the result has exactly one `ComputationError`, its detail includes both
  the stats failure and latch quarantine detail, and the `.corrupt` sidecar
  preserves the bad bytes.
- Add a monitor unit test that seeds a valid latch with one non-`ComputationError`
  cause, triggers a stats failure, and asserts both the returned state and saved
  latch contain the original cause plus exactly one `ComputationError`.
- Keep existing monitor tests passing for probe failures, mountinfo failures,
  stats failures, corrupt acked-stats, offline/non-btrfs, and healthy no-alert
  paths.
- Run `just test-rust`.
- Run `just test-vm braid-monitor` if the implementation touches any
  corrupt-latch wording used by the VM assertions.

## Assumptions

- This is a refactor only; behavior changes are not desired.
- The current detail wording should remain stable except where folding is
  centralized.
- No documentation update is needed unless implementation changes externally
  visible semantics.
