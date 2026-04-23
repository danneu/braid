# Fix `braid enroll --generate` Preflight

## Summary

Keep the April 23 `VerifyOutcome` fix intact and fix `--generate` at the caller. The bug is that `--generate` reuses the existing-keyfile planning path, which probes `verify_key_file()` against a file that does not exist yet. The fix is to keep one planner but make its mode explicit:

- `ExistingKeyfile`: verify passphrase, probe whether the keyfile is already enrolled, then check slot 1
- `GenerateNew`: verify passphrase, skip keyfile probe, check slot 1 only, then generate the file, then enroll

User-facing behavior after the fix:

- `braid enroll DIR --generate` succeeds on a fresh directory instead of failing with `Failed to open key file`
- wrong passphrase and slot-1 conflicts still fail before creating `braid.key`
- `braid enroll DIR --generate --dry-run` still exits before passphrase reads or keyfile creation
- non-`--generate` idempotent re-enroll behavior stays unchanged

## Implementation Changes

- In `cli/src/enroll_key_file.rs`, keep a single planning entrypoint but add a small internal mode enum, e.g. `EnrollmentPlanMode::{ExistingKeyfile, GenerateNew}`
- Extract a shared passphrase-verification helper against the first candidate disk
- Update the planner so:
  - `ExistingKeyfile` keeps current behavior, including `verify_key_file()` and `AlreadyEnrolled` vs `NeedsEnroll`
  - `GenerateNew` never calls `verify_key_file()`; after passphrase verification it only checks slot 1 on each candidate and returns `NeedsEnroll` actions
- Update `cmd_enroll_key_file(...)` so the `generate` branch invokes the planner in `GenerateNew` mode before `generate_key_file(...)`, and the non-generate branch invokes `ExistingKeyfile`
- Keep `apply_enrollment(...)`, `compile_enroll_steps(...)`, CLI args, dry-run output, and `luks::verify_key_file()` semantics unchanged
- Do not special-case missing-file errors from `verify_key_file()`; the nonexistent file probe should simply stop happening in the `--generate` branch

## Test Plan

Use the existing failing VM test as the top-level regression, then add focused Rust tests to pin the mode split and command-level short-circuits.

- Keep `tests/cli/braid-enroll-generate.py` as the main end-to-end regression for `--generate`
- Add a Rust unit test in `cli/src/enroll_key_file.rs` for successful `generate=true` enrollment that deliberately does not seed any `CryptsetupTestKeyFile` mock; if the code regresses and probes the nonexistent file, the test fails with `MissingMock`
- Add a Rust unit test for `generate=true` + wrong passphrase that asserts:
  - the command returns the existing wrong-passphrase validation
  - no keyfile is created on disk
- Add a command-level Rust unit test for `generate=true, dry_run=true` that:
  - succeeds without passphrase input or any `CryptsetupTestKeyFile` mock
  - verifies the target `braid.key` path is still absent afterward
  - therefore proves `cmd_enroll_key_file()` still short-circuits before passphrase reads and file creation
- Keep the existing VM slot-conflict/no-file-created scenario as the regression for "preflight before generation"
- Run targeted verification:
  - `just test-rust`
  - `just test-vm braid-enroll-generate`
  - then `just test-all` as the broader confirmation pass

## Public Interfaces / Behavior

- No new CLI flags, config fields, or output formats
- Internal-only change to enroll planning structure and mode dispatch
- Observable behavior change: `braid enroll DIR --generate` now works on first use with a missing `braid.key`, while preserving preflight failure-before-generation semantics

## Assumptions

- The intended contract of `--generate` is unchanged: validate first, create `braid.key` only after validation succeeds
- Slot 1 remains the dedicated keyfile slot
- The April 23 `VerifyOutcome` change is correct and must remain in place; this fix must not revert or weaken that behavior
- No docs update is required because this restores intended behavior rather than changing user-facing design
