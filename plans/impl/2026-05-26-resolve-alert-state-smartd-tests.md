# Pin the smartd bridge in `resolve_alert_state` with unit tests

## Context

`resolve_alert_state` (`cli/src/status.rs:603-643`) is the read-only surface that
`status`, the JSON report, and the TUI use to read latched alert state. Its
smartd branch (lines 629-636) appends an `AlertCause::SmartdAlert` only when the
live smartd flag is active **and** the latch does not already carry one:

```rust
if smartd_active
    && !state.causes.iter().any(|c| matches!(c, AlertCause::SmartdAlert))
{
    state.causes.push(AlertCause::SmartdAlert);
}
```

The two sibling branches in this function have dedicated unit tests
(`resolve_alert_state_surfaces_corrupt_latch_as_computation_error` at
`status.rs:5827`, `resolve_alert_state_surfaces_cleanup_pending_as_computation_error`
at `:5851`). The smartd branch has **none** -- no `status.rs` test ever writes
the smartd flag, so both halves of the guard are unit-untested.

The gap matters because the branch is reachable in production, not dead code:
`monitor` persists `SmartdAlert` into the latch (`monitor.rs:113-148`, proven by
`cmd_monitor_latches_smartd_alert_when_mounted` at `monitor.rs:830`, which
asserts the saved latch is exactly `[SmartdAlert]`). When `status`/TUI later call
`resolve_alert_state` with the flag still set and that latch present, the dedup
guard is the only thing preventing a duplicate cause and a doubled banner line
(`status.rs:1100`). A regression dropping the `!...any(SmartdAlert)` guard would
pass every current Rust test; the flag-to-banner VM test does not assert
single-cause dedup against a pre-existing latch.

Intended outcome: two fast, structure-insensitive unit tests that fully pin the
guard's two outcomes (append-when-absent, dedup-when-present), matching the
style and conventions of the existing sibling tests. This is a Testing-only
change -- no production code is touched.

## Scope decision (considered and rejected)

A structural pivot -- having `resolve_alert_state` reuse `alert::merge_into_latch`
(`alert.rs:460`), which already dedups by `same_cause_key` -- was rejected. It
would change `ComputationError` semantics (all `ComputationError`s share one key
under `same_cause_key`, so the cleanup-pending append would start *replacing* a
latched `ComputationError` instead of appending) and would not cover the
corrupt-latch early-return path. That is a behavioral change disproportionate to
a Low/Testing finding; the hand-rolled guard at 629-636 is small and correct.
Keep the fix test-only.

## Change

Add two unit tests inside the existing `mod tests` block in
`cli/src/status.rs`, immediately after
`resolve_alert_state_surfaces_cleanup_pending_as_computation_error` (after
`status.rs:5875`). Both reuse infrastructure already imported/used in this test
module:

- `isolated_paths()` -- imported at `status.rs:1405`.
- `alert::save_alert_latch(&AlertState { causes: vec![...] }, &paths)` -- exact
  call pattern already used at `status.rs:4409`.
- `std::fs::write(paths.smartd_alert(), b"")` to set the flag -- mirrors
  `monitor.rs:831`.
- `resolve_alert_state(&paths)` -- same-module `pub(crate)` fn under test.

Each test must carry the three-section `//` preamble (Intent / Why it exists /
Scenario) required by AGENTS.md Test Conventions, matching the two existing
`resolve_alert_state_*` tests.

### Test 1 -- dedup (the finding's target, essential)

`resolve_alert_state_dedups_smartd_alert_against_latch`

- Setup: `save_alert_latch(&AlertState { causes: vec![AlertCause::SmartdAlert] }, &paths)`,
  then `std::fs::write(paths.smartd_alert(), b"")`.
- Act: `let state = resolve_alert_state(&paths);`
- Assert: `assert_eq!(state.causes, vec![AlertCause::SmartdAlert]);` (exact-match
  proves exactly one `SmartdAlert` -- no duplicate). Optionally also
  `assert!(state.active())`.
- Regression caught: dropping the `!...any(SmartdAlert)` guard pushes a second
  `SmartdAlert`, failing the exact-match.

### Test 2 -- append / between-cycle bridge (cheap completion of the branch)

`resolve_alert_state_appends_smartd_alert_when_latch_absent`

- Setup: no latch saved (latch absent -> `unwrap_or_default()` yields empty),
  then `std::fs::write(paths.smartd_alert(), b"")`.
- Act: `let state = resolve_alert_state(&paths);`
- Assert: `assert_eq!(state.causes, vec![AlertCause::SmartdAlert]);`
- Regression caught: removing/inverting the append (flag set but no cause added)
  fails this; the VM test covers flag-to-banner but is slow and integration-level
  -- this pins the contract at the function level in milliseconds.

Together these pin both outcomes of lines 629-636 with typed-slice assertions
(no message-substring matching), consistent with the project's typed-error
convention and the existing sibling tests.

## Critical files

- `cli/src/status.rs` -- add the two tests in `mod tests` (after `:5875`). Only
  file modified.

Reference (read-only, for the executor): `cli/src/alert.rs:14-32` (`AlertState`
derives `PartialEq`/`Eq`/`Default`; `AlertCause::SmartdAlert` is a unit variant),
`cli/src/alert.rs:292-298` (`smartd_alert_active` treats a regular file at
`paths.smartd_alert()` as active).

## Optional follow-up (out of scope, note only)

The corrupt-latch early-return also has a smartd sub-branch
(`status.rs:621-623`: unreadable latch + flag set -> push `SmartdAlert` after the
`ComputationError`). It is the same bridge pattern and is also unit-untested, but
the finding scoped to 629-636 and this lives in a different (error) path. Mention
to the user as a possible later addition; do not include in this change.

## Verification

- `just test-rust` -- runs `cargo test` for the `braid-cli` crate. Confirm both
  new tests are collected and pass.
- Sanity-check the regression value before/after: temporarily delete the
  `&& !state.causes.iter().any(...)` guard and confirm Test 1 fails (do not
  commit this); restore it. This proves the test actually pins the guard.
- No fixture refresh or VM run needed -- this is a pure Rust unit-test addition
  touching no parser-critical tool versions and no production code path.

## Follow Up

- Add a unit test for the corrupt-latch plus smartd-flag bridge in `cli/src/status.rs` so the unreadable-latch path also proves it appends `AlertCause::SmartdAlert` after the `ComputationError`.
