# Plan: pin dry-run recovery-mode block for `braid enroll`

## Context

`plan_enroll` (`cli/src/enroll_key_file.rs:618`) calls
`preflight::check_no_pending_operation(paths)` at line 628 -- before
any dry-run branching (line 650). This means both real-run and
dry-run paths are blocked today when `pending-op.json` exists, which
preserves the project invariant that dry-run is a faithful preview
of the real run.

Unlike Add/Remove/RemoveMissing/Replace -- which call
`check_no_pending_operation` at the dispatch level in `main.rs`
(`main.rs:509,560,591,622`) before deciding anything else -- enroll
routes the check through the `plan_enroll` function that both
dry-run and real-run flow through. That makes the dual-path
exposure unique to enroll and unit-testable in one place.

Only one test pins the behavior:
`cmd_enroll_blocked_in_recovery_mode` (`cli/src/enroll_key_file.rs:3253`),
and it uses `dry_run: false`. A regression introducing
`if !dry_run` around the recovery-mode check would silently allow
`braid enroll DIR --dry-run` to produce a preview against
possibly-stale `pool.json` membership. No test would catch it.

The fix is to add a sibling test that mirrors the existing one with
`dry_run: true`.

## Change

Add one new `#[test]` function in the `tests` module of
`cli/src/enroll_key_file.rs`, immediately after
`cmd_enroll_blocked_in_recovery_mode` (currently line 3293) and
before `cmd_enroll_apply_failure_does_not_write_pending_op_journal`
(currently line 3308).

Name: `cmd_enroll_dry_run_blocked_in_recovery_mode`.

Structure (mirrors lines 3253-3293):

- Three-section `//` line-comment preamble (Intent / Why it exists /
  Scenario) per the literal form documented in
  [`docs/testing.md`](../../docs/testing.md#preamble-literal--line-comment-form).
  Do not copy the existing test's `/* ... */` block-comment shape --
  that test predates the standardized line-comment form and is one
  of 38 legacy block-comment preambles still in this file; the
  14 newer preambles in this same file (including
  `cmd_enroll_apply_failure_does_not_write_pending_op_journal`
  directly below the parent test) use the line-comment form.
- Use the existing `isolated_paths()` helper.
- Build and write a `pending-op.json` journal with the same
  `crate::journal::build_journal(...)` + `crate::journal::write_journal(...)`
  pair already used in the existing test.
- Reuse `MockRunner::default()`, `enroll_fs(&[])`, and
  `enroll_make_membership(&[("d1", "/dev/disk/by-id/d1")])` -- same
  scaffolding as the existing test.
- Call `cmd_enroll_key_file(...)` with the same
  `EnrollKeyFileParams` shape but **`dry_run: true`**.
- Assert the returned error stringifies to contain
  `"interrupted operation"`, identical to the existing assertion.

Do **not** add an explicit "zero MountpointCheck calls" assertion.
`MockRunner::default()` returns `CmdError::MissingMock` on any
unmocked request (`cli/src/cmd.rs:1431`), so any code path that
reaches a subprocess would already surface as a different error and
fail the `contains("interrupted operation")` check. An explicit
counter would duplicate that guarantee and be structure-sensitive.

## Preamble text

The preamble must use the canonical `//` line-comment form from
`docs/testing.md` and explain *why this duplicate exists*, not just
restate the parent test:

```rust
// Intent: dry-run enroll is also blocked when a pending-operation
//   journal exists, just like a real run.
// Why it exists: dry-run is a faithful preview of the real run.
//   A regression that gated `check_no_pending_operation` behind
//   `if !dry_run` would let `braid enroll DIR --dry-run` produce a
//   preview against possibly-stale pool.json membership that the
//   real run would refuse -- violating the dry-run-as-faithful-preview
//   invariant. The sibling test
//   `cmd_enroll_blocked_in_recovery_mode` pins the real-run path;
//   this one pins the dry-run path.
// Scenario: an add was interrupted; pending-op.json exists. User
//   runs `braid enroll --dry-run` to see what would happen before
//   deciding whether to recover.
```

(Plain `--` is used in this preamble even though the surrounding
file mixes em-dashes and `--` in test preambles. New code defaults
to ASCII per the global CLAUDE.md style rule; the "file already
uses Unicode form" exception is for matching a uniformly-Unicode
file, not a mixed one.)

## Files modified

- `cli/src/enroll_key_file.rs` -- add one new `#[test]` function in
  the `tests` module.

No other files change. No production code changes.

## Verification

1. `just test-rust` -- the new test must pass.
2. Regression-resistance check: temporarily edit
   `cli/src/enroll_key_file.rs:628` to wrap the check in
   `if !dry_run { ... }`, re-run `just test-rust`, and confirm the
   new test fails with an assertion about the missing
   `"interrupted operation"` substring (while the existing
   `cmd_enroll_blocked_in_recovery_mode` continues to pass).
   Revert the edit.

No VM tests are needed -- the behavior under test is a pure
function path in the Rust CLI, fully exercisable through
`cmd_enroll_key_file`.
