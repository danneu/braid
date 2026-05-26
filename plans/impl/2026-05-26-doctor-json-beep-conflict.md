# Reject `doctor --json --beep` at parse time

## Summary

Change `braid doctor --json --beep` from "accepted but beep ignored" to a
clap usage error. The CLI contract becomes explicit: machine-readable JSON
output cannot be combined with an audible side-effect request.

## Key changes

- In `DoctorArgs`, add a clap conflict on `--beep`:

  ```rust
  #[arg(long, conflicts_with = "json")]
  beep: bool,
  ```

- Keep the existing `doctor.rs` JSON-first beep suppression guard as
  defense-in-depth, but update nearby comments or tests if they imply the CLI
  still accepts `--json --beep`.
- Update `docs/commands/doctor.md`:
  - Remove wording that says `--beep` is ignored when combined with `--json`.
  - State that `--json` and `--beep` conflict.
  - Keep the invariant that JSON mode has no audible side effects.
- Update `docs/design/decisions/014-alerts.md`, in "Audible doctor beep is
  opt-in", so the architecture authority states that `--json` and `--beep`
  conflict at parse time while JSON doctor output remains side-effect-free.

## Test plan

- Add in-module parser tests in `cli/src/main.rs` near the existing clap
  relationship tests:
  - `doctor --json` parses with `json == true` and `beep == false`.
  - `doctor --beep` parses with `beep == true` and `json == false`.
  - `doctor --json --beep` rejects with `ErrorKind::ArgumentConflict`.
- Preserve or adjust the existing `doctor.rs` unit test for JSON-mode beep
  suppression so it still proves the beep wrapper is not invoked when
  `DoctorOptions.json` is true.
- Update `tests/cli/braid-doctor-beep.nix` and
  `tests/cli/braid-doctor-beep.py`:
  - Revise the test preambles to describe parse-time rejection instead of
    accepted JSON-mode suppression for `--json --beep`.
  - Change the JSON-plus-beep subtest to assert
    `braid doctor --json --beep` exits with clap usage error code 2 and does
    not create `/tmp/beep-invoked`.
  - Keep the existing plain `doctor`, `doctor --beep` success, and
    `doctor --beep` failure coverage.
- Run `just test-rust`.
- Run `just test-vm braid-doctor-beep`.

## Assumptions

- This is an intentional public CLI tightening, not a backwards-compatibility
  concern.
- Do not change `doctor --json` output shape or ordinary `doctor --beep`
  behavior.
