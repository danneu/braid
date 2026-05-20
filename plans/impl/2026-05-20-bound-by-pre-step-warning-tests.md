# Plan: BoundBy pre-step unit tests with byte-exact WARNING capture

## Context

The `run_lock_pre_steps` function at `cli/src/lock.rs:995-1033` runs two
pre-umount lifecycle steps when `cfg.systemd_lifecycle()` is true:

1. Silently stop three known scrub units (`let _ = ...`).
2. Iterate `list_bound_by("braid-online.service")` and stop each
   non-scrub consumer, emitting a `braid: WARNING:` line if the stop
   fails.

The plan at `plans/impl/2026-05-19-rust-owned-pool-operation-lock.md`
called out four Rust unit tests for the BoundBy branch
(`bound_by_pre_step_skips_three_scrub_units`,
`bound_by_pre_step_warns_on_nonzero_stop`,
`bound_by_pre_step_silently_continues_when_list_bound_by_errs`,
`bound_by_pre_step_handles_empty_bound_by_property`) plus a
companion `scrub_stop_pre_step_swallows_missing_unit`, and asked for
the silent-vs-warn asymmetry to be encoded in two distinct helpers so
a future caller cannot collapse them. None of the tests and neither
helper were committed. ADR 018:131-134 also documents the
scrub-stops-first-then-BoundBy lock ordering as part of the systemd
lifecycle contract.

The two existing tests `cmd_lock_skips_lifecycle_pre_steps_when_lifecycle_disabled`
and `cmd_lock_runs_lifecycle_pre_steps_when_lifecycle_enabled` mock
`SystemctlShowBoundBy` with empty stdout (via `lock_ok_raw`, which
returns `stdout: String::new()` -- see `cli/src/test_fixtures/mount.rs:71-78`),
so the per-unit stop branch is never entered under unit test. The VM
canary `tests/module/lock-stops-bound-consumers.py` stops a
cleanly-running consumer in cycle 1 (exit 0 -- no WARNING path) and
sees the consumer already inactive in cycle 2 (no-op stop). A
regression that dropped the WARNING text, warned on scrub stops, or
returned early on `list_bound_by` Err would still pass the VM canary
and the existing unit tests.

Outcome: lock down both WARNING wording branches (the `SystemctlStop`
exit-code form and the generic `({other})` Display form), the
silent-vs-warn asymmetry between scrub stops and BoundBy stops, the
scrub-skip filter inside the BoundBy loop, and the `list_bound_by`
Err -> silent return at unit-test resolution.

## Approach

Three coordinated changes:

1. **Refactor `run_lock_pre_steps` to accept `out: &mut dyn Write`** so
   the two existing `eprintln!` warnings flow through a writer. Tests
   pass a `Vec<u8>` sink and assert byte-exact content. Production
   call site at `cli/src/lock.rs:984` passes `&mut std::io::stderr()`.
   Matches the existing precedent at `cli/src/status.rs:766`
   (`emit_paused_balance_warning`), invoked in production with
   `&mut std::io::stderr()` (`cli/src/unlock.rs:174`) and in tests
   with a `Vec<u8>` sink (`cli/src/status.rs:2507`).

2. **Split the silent-vs-warn semantics into two module-private
   helpers in `cli/src/lock.rs`:**
   - `fn stop_unit_silent(online_ops: &dyn OnlineStateOps, unit: &str)`
     -- `let _ = online_ops.systemctl_stop(unit, false);`. Doc comment
     explains why: when `autoScrub` is disabled the three scrub units
     do not exist, and warning on every lock would emit three spurious
     WARNING lines.
   - `fn stop_unit_warn_on_error(online_ops: &dyn OnlineStateOps, out: &mut dyn Write, unit: &str)`
     -- holds the current match-on-`OnlineError` body, calling
     `writeln!(out, ...).ok()` for both branches. Doc comment explains
     why: user-declared BindsTo consumers (smbd, nfs, syncthing, ...)
     must be visibly warned about so the operator knows the umount
     might fail.

   `run_lock_pre_steps` becomes a flat dispatch: the scrub loop calls
   `stop_unit_silent`, the BoundBy loop calls
   `stop_unit_warn_on_error`. The asymmetry is encoded in the function
   names; a future caller picking the wrong helper is visible at the
   call site.

3. **Add a test-only cloneable failure type plus setters and per-unit
   error injection to `RecordingOnlineStateOps`** at
   `cli/src/online_state.rs:314-388`. `OnlineError` is not `Clone` and
   cannot be made so without losing its `std::io::Error` field
   (`OnlineError::Chmod { source: std::io::Error }` at
   `cli/src/online_state.rs:73-76`) and `CmdError` source (`CmdError`
   has no `Clone` impl at `cli/src/cmd.rs:1201-1207`). Introduce a
   `#[cfg(test)] #[derive(Debug, Clone)] pub enum StagedOnlineFailure`
   covering the three variants the recorder needs to fabricate:

   ```rust
   #[cfg(test)]
   #[derive(Debug, Clone)]
   pub enum StagedOnlineFailure {
       Spawn(String),
       SystemctlShow { unit: String, exit_code: i32, stderr: String },
       SystemctlStop { unit: String, exit_code: i32, stderr: String },
   }

   #[cfg(test)]
   impl StagedOnlineFailure {
       fn into_online_error(self) -> OnlineError { /* match arms */ }
   }
   ```

   `Spawn(msg)` lifts to `OnlineError::Spawn { source: CmdError::Failed(msg) }`,
   matching the production conversion chain in
   `RealOnlineStateOps::systemctl_stop` at `cli/src/online_state.rs:182-189`
   (`map_err(|source| OnlineError::Spawn { source })`).

   Recorder fields and setters:
   - Change `bound_by` field type from
     `RefCell<Result<Vec<String>, String>>` to
     `RefCell<Result<Vec<String>, StagedOnlineFailure>>`.
   - Add field `systemctl_stop_errs:
     RefCell<HashMap<String, StagedOnlineFailure>>`, initialized
     empty.
   - `pub fn set_bound_by_ok(&self, units: Vec<String>)`.
   - `pub fn set_bound_by_err(&self, failure: StagedOnlineFailure)`.
   - `pub fn set_systemctl_stop_err(&self, unit: &str, failure: StagedOnlineFailure)`.
   - In `list_bound_by`, clone the stored `Result` and `map_err` via
     `StagedOnlineFailure::into_online_error` to produce the real
     `OnlineError` the helper expects.
   - In `systemctl_stop`, after pushing the call record, look up the
     unit in `systemctl_stop_errs` and convert the staged failure to
     `OnlineError` if present; else `Ok(())`.
   - In `list_bound_by`, also push a `list_bound_by {unit}` line into
     `calls` before reading the staged result, so tests can assert
     the pre-step actually reached the BoundBy lookup path. The
     existing `mark_online_*` / `mark_offline_*` tests only inspect
     `calls.contains(&"mountpoint".into())` etc., so adding a new
     entry is additive (no existing test asserts the inverse).

   These additions are `#[cfg(test)]`-gated like the rest of the type.

The six new tests call `run_lock_pre_steps` directly (it is private
to the module, but the tests live in the same module's
`#[cfg(test)] mod tests`, so they have access). This is much lighter
than driving the full `cmd_lock_impl` pipeline, which would require
staging mountpoint check, umount, mapper close, btrfs scan, etc.

## Files

### Modified

- `cli/src/lock.rs`
  - Add `use std::io::Write;` import.
  - Change `run_lock_pre_steps(cfg, online_ops)` -> `run_lock_pre_steps(cfg, online_ops, out: &mut dyn Write)`.
  - Change call site at line 984 to
    `run_lock_pre_steps(config, &online_ops, &mut std::io::stderr());`.
  - Extract two helpers `stop_unit_silent` and
    `stop_unit_warn_on_error` (each with a `///` doc comment per
    AGENTS.md doc-comment rules).
  - Replace inline match body with helper call.
  - Add six `#[test]` functions in `mod tests` (see Tests section).

- `cli/src/online_state.rs`
  - Add `use std::collections::HashMap;` (and verify `CmdError` is
    already imported via `use crate::cmd::{CmdError, ...}` at line 7).
  - Add `#[cfg(test)] pub enum StagedOnlineFailure` with three
    variants (`Spawn`, `SystemctlShow`, `SystemctlStop`) and a
    `fn into_online_error(self) -> OnlineError` impl.
  - Change `RecordingOnlineStateOps.bound_by` field type from
    `RefCell<Result<Vec<String>, String>>` to
    `RefCell<Result<Vec<String>, StagedOnlineFailure>>`. Keep the
    default `Ok(Vec::new())` initial value.
  - Add field `systemctl_stop_errs:
    RefCell<HashMap<String, StagedOnlineFailure>>`, initialized
    empty.
  - Update `list_bound_by` impl to (a) push
    `format!("list_bound_by {unit}")` to `calls`, then (b) clone the
    stored `Result` and `map_err(StagedOnlineFailure::into_online_error)`.
  - Update `systemctl_stop` impl: after pushing the call record, look
    up `unit` in `systemctl_stop_errs` and convert staged failure if
    present, else `Ok(())`.
  - Add setters `set_bound_by_ok`, `set_bound_by_err`,
    `set_systemctl_stop_err`.

### New

None. All changes are edits to existing files.

## Tests

Add six `#[test]` functions to `cli/src/lock.rs` `mod tests`. Each
carries the three-section preamble from the project's test conventions
(Intent / Why it exists / Scenario).

For each test, drive `run_lock_pre_steps` directly with a
`RecordingOnlineStateOps`, a lifecycle-enabled `Config`, and a
`Vec<u8>` writer sink; assert on the ops' `calls()` log and on
`String::from_utf8(buf)` of the writer.

### `bound_by_pre_step_skips_three_scrub_units`

- Stage `set_bound_by_ok(vec!["braid-scrub.timer".into(),
  "braid-scrub.service".into(),
  "braid-scrub-resume-trigger.service".into(),
  "smbd.service".into(), "nfs-server.service".into()])`.
- Stage `systemd_lifecycle = true`.
- Run `run_lock_pre_steps`.
- Assert the recorder's `calls()` shows the full ordered sequence:
  three scrub-phase stops, then `list_bound_by braid-online.service`,
  then exactly two BoundBy-phase stops -- `smbd.service` and
  `nfs-server.service` -- in that order. No stop entries for the
  scrub units appear after the lookup.
- Assert the writer sink is empty (no WARNING for the successful
  stops).

### `bound_by_pre_step_warns_on_nonzero_stop`

- Stage `set_bound_by_ok(vec!["smbd.service".into()])`.
- Stage `set_systemctl_stop_err("smbd.service",
  StagedOnlineFailure::SystemctlStop { unit: "smbd.service".into(),
  exit_code: 5, stderr: "stop failed".into() })`.
- Run `run_lock_pre_steps`.
- Assert the writer sink equals exactly
  `"braid: WARNING: failed to stop smbd.service (exit 5) -- continuing; umount may fail\n"`.
  Byte-exact: the test locks the literal string including the
  trailing `\n` (`writeln!` adds it), the `--`, and the `(exit N)`
  form.
- Assert the function returned (did not panic) and the recorder
  shows the stop attempt for `smbd.service`.

### `bound_by_pre_step_warns_on_spawn_error`

- Stage `set_bound_by_ok(vec!["smbd.service".into()])`.
- Stage `set_systemctl_stop_err("smbd.service",
  StagedOnlineFailure::Spawn("boom".into()))`.
- Run `run_lock_pre_steps`.
- Assert the writer sink equals exactly
  `"braid: WARNING: failed to stop smbd.service (command failed: boom) -- continuing; umount may fail\n"`.
  Byte-exact: the generic `other => writeln!(out, "... ({other}) ...")`
  arm prints the full Display of `OnlineError`. For
  `OnlineError::Spawn { source: CmdError::Failed("boom") }` the
  Display chain is `OnlineError::Spawn`'s `#[error("{source}")]`
  delegating to `CmdError::Failed`'s `#[error("command failed: {0}")]`,
  yielding the literal `command failed: boom` between the parens.
  Pins the generic branch wording so a regression that changed the
  match (e.g. collapsed both arms, dropped the `({other})` field,
  reordered the prefix) would fail.
- This test pairs with `bound_by_pre_step_warns_on_nonzero_stop`:
  together they cover both arms of the `match e` in the
  `stop_unit_warn_on_error` helper.

### `bound_by_pre_step_silently_continues_when_list_bound_by_errs`

- Stage `set_bound_by_err(StagedOnlineFailure::SystemctlShow {
  unit: "braid-online.service".into(), exit_code: 1,
  stderr: String::new() })`.
- Run `run_lock_pre_steps`.
- Assert the recorder's `calls()` contains the three scrub stops and
  one `list_bound_by braid-online.service` entry, then nothing else
  (no `stop <smbd|nfs-server>` -- the early-return on `list_bound_by`
  Err short-circuited the loop).
- Assert the writer sink is empty. The early-return on
  `list_bound_by` Err is silent -- it matches the original wrapper's
  `2>/dev/null || true` on `systemctl show -P BoundBy`.

### `bound_by_pre_step_handles_empty_bound_by_property`

- Stage `set_bound_by_ok(Vec::new())` -- no consumers declared
  `BindsTo=braid-online.service`.
- Run `run_lock_pre_steps`.
- Assert the recorder's `calls()` contains the three scrub stops and
  one `list_bound_by braid-online.service` entry, then nothing else.
- Assert the writer sink is empty.
- This is the positive companion to the Err test: the trait split
  (`Ok(empty)` vs `Err`) exists precisely so the two cases are
  distinguishable, and this test pins the distinction.

### `scrub_stop_pre_step_swallows_missing_unit`

- Stage `set_systemctl_stop_err("braid-scrub.timer",
  StagedOnlineFailure::SystemctlStop {
  unit: "braid-scrub.timer".into(), exit_code: 5,
  stderr: "Unit braid-scrub.timer not loaded.".into() })`. (Models a
  CLI-only / autoScrub-disabled host where the scrub unit is absent
  but `systemd_lifecycle = true`.)
- Stage `set_bound_by_ok(vec!["smbd.service".into()])`.
- Run `run_lock_pre_steps`.
- Assert the writer sink contains zero `braid: WARNING: failed to
  stop braid-scrub.timer` lines (the scrub-stop helper must swallow
  the Err silently).
- Assert the recorder's `calls()` includes the
  `stop braid-scrub.timer no_block=false` attempt (so we know the
  call was made and the Err was returned), then the two remaining
  scrub stops, then `list_bound_by braid-online.service`, then the
  BoundBy `stop smbd.service` -- i.e. the scrub failure did not
  short-circuit the rest of the pre-step pipeline (ADR 018:131-134
  lock ordering still holds).
- Locks the `stop_unit_silent` helper's contract independently of
  `stop_unit_warn_on_error` -- a future refactor that picked the
  warn-helper for the scrub loop would fail this test.

## Verification

End-to-end smoke checklist:

1. `just test-rust` -- the six new tests pass; the two existing
   `cmd_lock_*_lifecycle_pre_steps_*` tests still pass (the writer
   refactor changed nothing observable through `cmd_lock_impl`); the
   existing `list_bound_by_parses_whitespace_separated_units` and the
   `RecordingOnlineStateOps`-backed `mark_online_*` / `mark_offline_*`
   tests in `cli/src/online_state.rs` still pass (the recorder field
   type change and the new `list_bound_by` call recording are
   internal -- existing tests only `calls.contains(...)` known entries,
   so adding new entries is additive).
2. `just test-vm lock-stops-bound-consumers` -- the VM canary still
   passes unchanged. The refactor preserves the production
   `eprintln!`-equivalent behavior (now `writeln!(stderr, ...)`),
   and the VM does not assert on stderr content.

Cross-check by manually breaking the implementation in a scratch
worktree:

- Drop the scrub-skip `matches!` guard in the BoundBy loop -- expect
  `bound_by_pre_step_skips_three_scrub_units` to fail with the three
  scrub-unit names appearing in the BoundBy stop log.
- Change the WARNING string (e.g. swap `--` for em-dash or drop the
  `(exit N)`) -- expect `bound_by_pre_step_warns_on_nonzero_stop` to
  fail with a byte-exact diff.
- Change the generic `({other})` arm to `({other:?})` or drop the
  parens -- expect `bound_by_pre_step_warns_on_spawn_error` to fail
  with a byte-exact diff.
- Change the `let Ok(bound_by) = ... else { return; }` to
  `let bound_by = online_ops.list_bound_by(...).expect("...")` --
  expect
  `bound_by_pre_step_silently_continues_when_list_bound_by_errs` to
  fail by panicking through the test.
- Swap the scrub loop's `stop_unit_silent` call for
  `stop_unit_warn_on_error` -- expect
  `scrub_stop_pre_step_swallows_missing_unit` to fail with the
  scrub-unit WARNING line in the writer sink.

## Out of scope

- `bound_by_pre_step_parses_whitespace_separated_units` with four
  variant fixtures (the plan's 7th BoundBy-related test). Deferred:
  parser edges partially overlap with
  `list_bound_by_parses_whitespace_separated_units` already at
  `cli/src/online_state.rs:424`; the missing variants
  (empty stdout, trailing newline, Ok(exit=1) -> `SystemctlShow` Err,
  `CmdError::Failed` -> `Spawn` Err) are best added in a follow-up
  scoped to `RealOnlineStateOps::list_bound_by` directly, not the
  pre-step.
- Renaming or relocating `run_lock_pre_steps`. It stays
  module-private; tests reach it through `mod tests`.
- Touching `cmd_lock_impl` other than threading the
  `&mut std::io::stderr()` argument.
- Making `OnlineError` cloneable. Doing so would force a `String`
  round-trip of `std::io::Error` and `CmdError::Failed`, losing
  source-error fidelity in production for a test-only convenience.
  The `StagedOnlineFailure` shim keeps production types intact.
