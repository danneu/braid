# Make `braid doctor` Beep Opt-In

## Summary

Change the doctor alert-beep check so plain `sudo braid doctor` never plays an audible beep. Add `sudo braid doctor --beep` as the explicit opt-in path for testing the real alert sound. Keep `--json` silent even when combined with `--beep`.

## Key Changes

- Add a `--beep` flag to `DoctorArgs`.
- Introduce named options structs instead of adding more positional booleans:
  - `DoctorOptions { json, beep }` for `cmd_doctor` and `run_doctor`.
  - `BeepCheckOptions { is_root, json_output, play_beep }` for `check_beep_path_inner`.
- Preserve existing config validation behavior for `beep_path`: missing notifier config, malformed config, disabled beep monitoring, and non-root handling stay the same.
- After confirming beep monitoring is configured, change the default non-JSON/non-`--beep` branch to `Skip` without invoking `BraidBeepProbe`.
- Use these exact user-facing messages:
  - Default skip: `skipped (pass --beep to play the audible alert test beep)`
  - JSON skip: `skipped in --json mode -- rerun with --beep without --json to play the alert test beep`
  - `--beep` success: `alert test beep command succeeded -- you should have heard a 1 kHz, 500 ms disk-alert beep`
  - Keep the existing failure message shape for broken beep probes, since it contains useful diagnostics.
- Update all user-facing and test-intent copy that claims plain `braid doctor` plays or includes the beep test. At minimum this includes `manual/commands/doctor.md`, `manual/commands/monitor.md`, `tests/cli/braid-doctor-beep.py`, and `tests/cli/braid-doctor-beep.nix`.
- Do not edit `manual/book/`; it is ignored/generated.

## Tests

- Update unit tests in `cli/src/doctor.rs`:
  - Add or adjust a default-off test proving `play_beep = false` returns `Skip` and does not invoke the runner.
  - Update success and failure tests to pass `play_beep = true`.
  - Keep JSON tests proving no runner invocation, including with `play_beep = true`.
  - Update expected message assertions to match the new copy.
- Update `tests/cli/braid-doctor-beep.py`:
  - Plain `braid doctor` with `/tmp/beep-broken` present exits 0 and reports `beep_path` as `skip`.
  - `braid doctor --beep` succeeds when mock beep is healthy.
  - `braid doctor --beep` exits 1 when mock beep is broken.
  - `braid doctor --json --beep` still skips and exits 0 without invoking the wrapper.
- Update `tests/cli/shell-completion.py` with a `doctor` flag completion assertion that includes `--json` and `--beep`.
- Run `cargo test -p braid-cli beep_path` for focused unit coverage, then `just test-vm braid-doctor-beep` and `just test-vm shell-completion` for live CLI behavior.

## Assumptions

- `--json` has precedence over `--beep`; JSON output must never produce audible side effects.
- `beep_path` remains a `Skip`, not `Warn`, when the operator does not pass `--beep`.
- No backwards compatibility shim is needed because braid is unreleased.
