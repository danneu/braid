# Plan: cover the empty-stderr arm of both stderr-suffix formatters

## Context

An `/ultrareview` finding (Low / Testing) flagged that the
`nonzero-exit + empty-stderr` branch of `format_systemctl_stop_failure`
(`cli/src/ack.rs:244-249`) has no unit test. That branch emits a distinct
message -- `"warning: systemctl stop braid-alert.service: {status}"` with no
trailing `: {stderr}` diagnostic. A regression that swapped the empty/non-empty
arms or dropped the status would ship a useless beeper-stop warning and pass CI.

Verifying the finding surfaced two refinements:

1. **The finding's proposed assertion is too weak.** `Some` + "contains the
   status" passes even if the arms are swapped (with empty stderr the swapped
   arm produces `"...exit status: 5: "`, which still contains the status) and
   passes if the message loses its `warning: systemctl stop ...` prefix. Since
   these are user-visible output boundaries, the test should lock the **exact**
   rendered string (`docs/dev/testing.md:72-81`), which catches a dropped
   status, a lost prefix, and an arm swap at once.
2. **The same gap recurs in a sibling.** `output_to_raw`'s signal-killed branch
   (`cli/src/cmd.rs:1270-1274`) uses the identical "trim stderr; empty -> prefix,
   else `prefix: stderr`" idiom. Its only test
   (`output_to_raw_signal_killed_returns_error`, `cmd.rs:2922`) feeds non-empty
   stderr (`b"partial output"`), so its empty-stderr arm (`cmd.rs:1271`) is also
   untested.

Outcome: close both empty-stderr coverage gaps with pure-function tests that
assert the exact user-facing output. No production code changes.

## Approach (chosen: targeted tests, both sites)

Add one `#[cfg(unix)]` unit test per site, each mirroring the existing
sibling test in that file but feeding **empty** stderr and asserting the
**exact** rendered string (the full warning for `ack.rs`, the full
`CmdError::Failed` Display for `cmd.rs`). Exact-match is the documented choice
for user-visible Display/output boundaries (`docs/dev/testing.md:72-81`) and
subsumes weaker `contains` / `!ends_with(": ")` checks.

Both preambles use the canonical contiguous `//` line-comment form with exact
`Intent` / `Why it exists` / `Scenario` labels (`docs/dev/testing.md:11-22`) --
not the block-comment style some existing `ack.rs` tests drifted to, nor the
abbreviated `// Why:` label some `cmd.rs` tests use.

Rejected alternatives (for the record, do not implement):

- **Cited site only:** leaves the identical `cmd.rs` sibling gap open for a
  reviewer to refile.
- **Extract a shared `pub(crate)` stderr-suffix helper:** the idiom appears in
  exactly two places, in different modules with different return types
  (`Option<String>` vs `CmdError`); extraction is below the rule-of-three
  threshold and trades production-code risk to close a Low test gap without
  shrinking the test surface (each call site still needs a prefix-wording test).

## Changes

### 1. `cli/src/ack.rs` -- new test in `mod tests`

Add after `format_systemctl_stop_failure_warns_on_nonzero_exit_with_stderr`
(currently ends at line 1940). Imports already present in the test module
(`ExitStatus`, `Output`, `ExitStatusExt` at lines 303-306).

```rust
// Intent: A non-zero `systemctl stop braid-alert.service` exit with empty
//   stderr still warns, rendering the process status with no trailing
//   diagnostic suffix.
// Why it exists: The empty-stderr arm formats a distinct message
//   ("...{status}" with no ": {stderr}" tail). A swapped empty/non-empty arm,
//   a dropped status, or a lost prefix would ship a malformed or useless
//   beeper-stop warning; only an empty-stderr input exercises that arm.
// Scenario: systemctl exits non-zero but prints nothing to stderr (the stop is
//   rejected with only an exit code), so braid must still surface the status.
#[cfg(unix)]
#[test]
fn format_systemctl_stop_failure_warns_on_nonzero_exit_without_stderr() {
    let output = Output {
        status: ExitStatus::from_raw(5 << 8),
        stdout: Vec::new(),
        stderr: Vec::new(),
    };

    assert_eq!(
        format_systemctl_stop_failure(&output),
        Some("warning: systemctl stop braid-alert.service: exit status: 5".to_string()),
    );
}
```

Expected string derivation: `format!("warning: systemctl stop braid-alert.service: {}", output.status)` with `ExitStatus::from_raw(5 << 8)` rendering as `exit status: 5` (the existing non-empty sibling asserts the same `exit status: 5` Display).

### 2. `cli/src/cmd.rs` -- new test in `mod tests`

Add after `output_to_raw_signal_killed_returns_error` (currently ends at line
2936). `ExitStatusExt` is imported at module scope (`cmd.rs:2`) and `libc` is
available (used by the sibling test).

```rust
// Intent: A signal-killed child with empty stderr still produces a
//   CmdError::Failed naming the signal, with no trailing diagnostic suffix.
// Why it exists: The empty-stderr arm formats a distinct detail
//   ("...({name})" with no ": {stderr}" tail). A swapped empty/non-empty arm,
//   a dropped signal name, or a lost command prefix would mislead debugging;
//   only an empty-stderr input exercises that arm.
// Scenario: OOM-killer sends SIGKILL to a child that wrote nothing to stderr
//   before dying -- braid must still report the signal.
#[test]
fn output_to_raw_signal_killed_empty_stderr_reports_signal() {
    use std::process::ExitStatus;

    let status = ExitStatus::from_raw(libc::SIGKILL);
    let output = std::process::Output {
        status,
        stdout: Vec::new(),
        stderr: Vec::new(),
    };
    let err = output_to_raw("cryptsetup luksOpen /dev/sda".into(), output).unwrap_err();
    assert_eq!(
        err.to_string(),
        "command failed: cryptsetup luksOpen /dev/sda: killed by signal 9 (SIGKILL)",
    );
}
```

Expected string derivation: `CmdError::Failed` is `#[error("command failed: {0}")]`
(`cmd.rs:1226`); the empty-arm detail is `format!("{cmd_str}: killed by signal {sig} ({name})")`
with `sig=9`, `name="SIGKILL"` (the existing sibling asserts `signal 9` / `SIGKILL`
for the same `from_raw(libc::SIGKILL)` status).

## Out of scope

- No production code changes to `format_systemctl_stop_failure` or
  `output_to_raw`.
- No helper extraction / unification.
- Existing sibling tests (and their drifted preamble styles) are left as-is;
  this plan only adds the two missing-branch tests.

## Verification

- `just test-rust` -- both new tests compile and pass; the existing
  non-empty-stderr / signal tests still pass. (CLI crate package is
  `braid-cli`; `just test-rust` runs `cargo test`.)
- Regression check (optional, manual): temporarily swap the empty/non-empty
  arms in either formatter and re-run `just test-rust`; the corresponding new
  test must fail the exact-match assertion. Any change to the empty-arm wording
  (arm swap, dropped status/signal, lost prefix, dangling colon) breaks the
  exact string. Revert the swap.
- No VM tests required -- both functions are pure and CPU-only; the existing
  `braid-smartd-alert.py` VM test already covers the live beeper-stop path.
