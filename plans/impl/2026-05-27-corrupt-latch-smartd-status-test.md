# Add Coverage for Corrupt Latch plus Smartd Flag

## Summary

Add one Rust unit test for the `resolve_alert_state` early-return path where
`alert-latch.json` is unreadable or corrupt and the live `smartd-alert` flag is
present. This is a test-only fix: production behavior is already correct, but
the error branch in `cli/src/status.rs` is not pinned.

## Key Changes

- In `cli/src/status.rs`, add a test inside the existing `#[cfg(test)] mod tests`,
  immediately after `resolve_alert_state_surfaces_corrupt_latch_as_computation_error`.
- Name it `resolve_alert_state_bridges_smartd_alert_when_latch_corrupt`.
- Use the existing test helpers and imports:
  - `isolated_paths()`
  - `std::fs::write(paths.alert_latch_json(), b"not json")`
  - `std::fs::write(paths.smartd_alert(), b"")`
  - `resolve_alert_state(&paths)`
- Assert the exact typed cause sequence:
  - first cause is `AlertCause::ComputationError { detail }`
  - second cause is `AlertCause::SmartdAlert`
  - `detail` contains `"alert latch unreadable"`
- Include the required three-section `//` preamble:
  - Intent: corrupt latch still surfaces live smartd flag.
  - Why it exists: the unreadable-latch path returns early and manually appends
    smartd state, separate from the normal dedup branch.
  - Scenario: latch bytes are corrupt while smartd has written its alert flag;
    operator runs `braid status`.

## Public APIs / Interfaces

No public API, CLI behavior, JSON schema, or production logic changes.

## Test Plan

- Run `just test-rust`.
- Optional regression proof: temporarily remove the
  `if smartd_active { causes.push(AlertCause::SmartdAlert); }` block in the
  `Err(e)` branch, confirm the new test fails, then restore before finalizing.

## Assumptions

- Keep the test scoped to corrupt latch plus smartd flag only; do not add
  cleanup-pending to this case.
- Do not rewrite `resolve_alert_state` or refactor through `merge_into_latch`.
- Do not add VM coverage; this is a focused unit-test gap.
