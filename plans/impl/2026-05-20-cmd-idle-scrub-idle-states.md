# Plan: pin cmd_idle wiring for the three untested "idle" scrub states

## Context

`cmd_idle` (in `cli/src/idle.rs:87-104`) classifies `ScrubState`
results from `btrfs scrub status` into the autosuspend gate's
`IdleResult`. It groups four arms together as "idle":

```rust
ScrubState::Never
| ScrubState::Finished { .. }
| ScrubState::Aborted { .. }
| ScrubState::Interrupted { .. } => IdleResult::Idle,
```

The unit suite in `cli/src/idle.rs#tests` exercises this branch only
via `ScrubState::Finished` (through `idle_runner_with_scrub_finished()`
in `idle_when_all_ops_quiet`). `Never`, `Aborted`, and `Interrupted` are
not exercised through `cmd_idle`.

Parser-level tests in `cli/src/parse/btrfs_scrub_status.rs` pin the
parser-side mapping of stdout to each `ScrubState` variant, but they do
not pin the `cmd_idle` wiring. A refactor that moves `Aborted` or
`Interrupted` into the `Unknown` arm -- or drops `Never` from the list
when adding a new arm -- compiles cleanly (the match is exhaustive) and
parser tests stay green, but autosuspend silently flips those states to
"block suspend." The fail-closed direction means the regression is
operationally invisible: autosuspend just stops working.

The fixtures needed to close the gap already exist in
`cli/src/test_fixtures/scrub.rs:72-119` (`scrub_status_never`,
`scrub_status_aborted`, `scrub_status_interrupted`) and are already
re-exported from `cli/src/test_fixtures.rs:201-205`. Only three small
test functions and one import-line addition are needed.

The sibling pattern lives in `cli/src/scrub_needs_resume.rs:52-110`,
which writes one unit test per `ScrubState` variant ("aborted_needs_resume",
"never_does_not_need_resume", ...). The new idle-side tests should mirror
that one-test-per-variant style and the existing `idle_when_*` naming
convention in `cli/src/idle.rs`.

## Files to modify

- `cli/src/idle.rs` -- add three tests inside the existing `#[cfg(test)] mod tests` block; extend the existing `use crate::test_fixtures::{...}` import on lines 115-119 with `scrub_status_aborted, scrub_status_interrupted, scrub_status_never`.

No production-code changes. No new fixtures. No new helpers.

## Implementation

1. Edit the test-module import in `cli/src/idle.rs` (currently lines
   115-119) to add `scrub_status_aborted`, `scrub_status_interrupted`,
   and `scrub_status_never` alongside the existing `scrub_status_unknown`.

2. Add three new `#[test]` functions inside the `tests` module, sited
   next to `idle_when_all_ops_quiet` (around line 137) so all four
   "scrub state -> Idle" wiring tests live together. Each test follows
   the project's three-section preamble convention (Intent / Why /
   Scenario) and the body shape of `idle_when_all_ops_quiet`:

   ```rust
   // Intent: <ScrubState> through cmd_idle yields Idle.
   // Why it exists: Pins the cmd_idle wiring from the parser-side
   //   ScrubState variant to IdleResult::Idle. The match in cmd_idle
   //   groups Never/Finished/Aborted/Interrupted into a single arm;
   //   the parser tests only prove ScrubState classification, not
   //   this wiring. A refactor that moves a variant into the Unknown
   //   arm compiles cleanly and parser tests stay green, but
   //   autosuspend silently stops working.
   // Scenario: <real-world story for this variant>.
   #[test]
   fn idle_when_scrub_<variant>() {
       let (scrub_req, scrub_out) = scrub_status_<variant>();
       let runner = MockRunner::default().with_output(scrub_req, scrub_out);
       let fs = IdleMockFs::with_exclop("none");

       let result = cmd_idle(&runner, &fs, &idle_mp());
       assert_eq!(result, IdleResult::Idle);
   }
   ```

   The `Why it exists:` (not `Why:`) wording is the documented form in
   [`docs/testing.md`](../../docs/testing.md) (lines 16-19) and the most
   recent test added to this file (`busy_unknown_on_scrub_state_unknown`,
   `cli/src/idle.rs:418-422`) already uses it. The older `// Why:`
   short form that appears in several siblings (e.g.
   `idle_when_all_ops_quiet`, `busy_when_scrub_running`) predates the
   convention; new tests should not perpetuate it.

   Concrete scenarios per variant:

   - `idle_when_scrub_never`: a freshly-created pool that has never
     been scrubbed.
   - `idle_when_scrub_aborted`: a scrub previously cancelled by
     `braid lock`, leaving resumable progress on disk; sysfs is quiet.
   - `idle_when_scrub_interrupted`: a userspace scrub process died
     before completing; sysfs is quiet.

3. Match the style choices of `idle_when_all_ops_quiet`: inline the
   two-line runner setup (no new helper), seed sysfs with
   `IdleMockFs::with_exclop("none")` so the sysfs branch lets execution
   reach the scrub probe, and assert only `IdleResult::Idle`. Do not
   add `runner.requests()` assertions -- the relevant invariant is the
   wiring decision, and over-specifying recorded requests would
   duplicate the contract already pinned by
   `no_balance_or_replace_subprocess_calls`.

## What this plan deliberately does NOT do

- Does not touch production code in `cli/src/idle.rs`. The four-arm
  match is correct as written; the gap is in coverage, not behavior.
- Does not introduce a parameterized table-driven test. The dominant
  pattern in this file (`busy_when_balance`, `busy_when_device_add`,
  ...) and the sibling pattern in `cli/src/scrub_needs_resume.rs` are
  per-variant test functions; table-driving would diverge for no
  readability win and would obscure which variant broke on failure.
- Does not add an `idle_runner_with_scrub_<variant>` helper per state.
  Three call sites do not justify the indirection; `busy_when_scrub_running`
  already inlines its own fixture call (`idle_scrub_running(45)`) and is
  the local precedent.
- Does not extract a shared "ScrubState -> IdleResult" classifier between
  `cmd_idle` and `cmd_scrub_needs_resume`. The two functions answer
  different questions about the same enum ("can we suspend?" vs "should
  we resume?"); a shared classifier would just rename the predicates.

## Verification

- `just test-rust` -- runs the CLI unit tests. The three new tests must
  pass on the existing code (they are coverage, not bug fixes); a
  failure of any of them would mean the cited four-arm match has
  already drifted.
- Eyeball the three test bodies once more after editing to confirm
  they each select a different fixture function -- the only thing that
  visibly distinguishes the three tests is the fixture call, so a
  copy-paste error there would silently re-test the same variant
  three times.
