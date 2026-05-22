# Plan: drop `UpsError::QueryFailedJsonReported`, use an outcome enum

## Context

`cli/src/ups.rs` defines a typed error variant `UpsError::QueryFailedJsonReported` whose only purpose is to signal "I already wrote a JSON sentinel to stdout; please exit 1 silently". It is a control-flow signal masquerading as an error. Specifically:

- The `Display` string is `"internal: ups query failed (json sentinel already on stdout)"` -- operator-confusing wording that is only kept out of stderr because the dispatch in `cli/src/main.rs` matches the variant first and exits before `print_cli_error` runs.
- The seam is `pub` (re-exported through `braid_cli::ups::UpsError`) and pinned by two unit tests, so any future caller that uses `?` or routes through `print_cli_error` would surface that string.
- Returning an `Err` to mean "everything went fine, I just emitted my own report" couples the command and the shell more than necessary.

The user-facing contract (JSON sentinel on stdout, silent stderr, exit 1) stays the same. Only the internal typing changes. No nixpkgs/tool versions move and no parser surface is touched, so this is a pure Rust-only refactor.

## Change

Introduce a local outcome enum and shift the JSON-reported case from `Err` to `Ok`:

```rust
/// Outcome of `cmd_ups_status` so the JSON-reported case stays an
/// `Ok` variant instead of a typed-`Err` control-flow sentinel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpsStatusOutcome {
    Done,
    JsonErrorReported,
}
```

This matches the existing `OpenOutcome` / `VerifyOutcome` pattern in `cli/src/luks.rs:554, 810`.

### Files to modify

1. **`cli/src/ups.rs`**
   - Add `pub enum UpsStatusOutcome { Done, JsonErrorReported }` with a `///` doc comment justifying it as the JSON-emission seam.
   - Delete the `UpsError::QueryFailedJsonReported` variant (current lines ~28-29).
   - Change `cmd_ups_status` return type from `Result<(), UpsError>` to `Result<UpsStatusOutcome, UpsError>`. The two success terminations at the end of the function return `Ok(UpsStatusOutcome::Done)`.
   - Change `print_not_enabled`, `emit_query_failed`, `emit_invocation_failed` return types from `Result<(), UpsError>` to `Result<UpsStatusOutcome, UpsError>`.
     - `print_not_enabled` always returns `Ok(UpsStatusOutcome::Done)`.
     - `emit_query_failed(json, detail)` returns `Ok(UpsStatusOutcome::JsonErrorReported)` in the `json` branch (after writing the JSON), `Err(UpsError::QueryFailed { detail })` otherwise.
     - `emit_invocation_failed(json, error)` returns `Ok(UpsStatusOutcome::JsonErrorReported)` in the `json` branch, `Err(UpsError::InvocationFailed { detail })` otherwise.
   - Update the two existing unit tests that pin the variant (current names: `cmd_ups_status_invocation_failure_json_returns_already_reported` and `emit_query_failed_json_returns_already_reported`) to assert `Ok(UpsStatusOutcome::JsonErrorReported)` instead of `Err(UpsError::QueryFailedJsonReported)`. Rename them to `..._returns_json_error_reported` and update the `// Intent / Why / Scenario` preambles to drop the `QueryFailedJsonReported` reference -- the contract under test is now the outcome variant, not an error variant. Keep one of the two tests focused on `emit_query_failed` directly and the other on `cmd_ups_status` end-to-end; both branches stay covered. Add a sibling test for `emit_invocation_failed` returning `Ok(UpsStatusOutcome::JsonErrorReported)` so both JSON-reported emit helpers are pinned symmetrically.

2. **`cli/src/main.rs`** (dispatch at current lines 994-1012)
   - Replace the existing three-arm match
     ```rust
     Ok(()) => {}
     Err(UpsError::QueryFailedJsonReported) => { std::process::exit(1); }
     Err(e) => { print_cli_error(&e.to_string()); std::process::exit(1); }
     ```
     with
     ```rust
     Ok(UpsStatusOutcome::Done) => {}
     Ok(UpsStatusOutcome::JsonErrorReported) => { std::process::exit(1); }
     Err(e) => { print_cli_error(&e.to_string()); std::process::exit(1); }
     ```
   - Keep using bare `std::process::exit(1)` for consistency with every other dispatch arm in `main.rs` (returning `ExitCode` from `fn main` would be inconsistent and out of scope).

### Files intentionally NOT changed

- **`manual/commands/ups-status.md`** documents only the user-facing contract (JSON on stdout, stderr silent, exit 1) and does not name the variant. No edit needed -- the contract is preserved.
- **`tests/cli/braid-status-ups.py`** (NixOS VM test) checks the user-facing contract, not the Rust variant. No edit needed.
- **No ADR / decision doc** edit needed -- ADR 026 is referenced by the finding only as a thematic analogy; this is not the same code path.

## Verification

1. **`just test-rust`** -- runs `cargo test`, including the updated `ups` unit tests. The renamed/retyped tests must pass; the new `emit_invocation_failed` sibling must pass.
2. **`cargo build`** (implicit in step 1) -- the dispatch in `main.rs` must compile with the new outcome enum; the exhaustive match prevents drift.
3. **`just test-vm braid-status-ups`** -- the NixOS VM test that exercises `braid ups status` (and `--json` failure paths) against a live `dummy-ups` driver. It pins the user-facing contract: JSON sentinel on stdout, exit 1, stderr silent. Behavior must be unchanged.

No fixture refresh, no parser-canary run, no full-suite VM run -- the change is internal Rust typing.
