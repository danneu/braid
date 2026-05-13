# Plan: Pin Recorded Requests in `no_balance_or_replace_subprocess_calls`

## Context

The regression test `no_balance_or_replace_subprocess_calls`
(`cli/src/idle.rs:336-351`) was added to guarantee that `cmd_idle` no
longer calls `BtrfsBalanceStatus`, `BtrfsReplaceStatus`, or
`BtrfsFilesystemShow` after the move to a sysfs-based exclusive-op scan.

The test's preamble promises:

> Adding a new caller of any of those CmdRequests inside `cmd_idle`
> would surface as MissingMock here.

But the body only asserts `assert_eq!(result, IdleResult::Idle)`. The
"surface as MissingMock" guarantee is implicit -- it relies on two
collaborating facts:

1. `idle_runner_with_scrub_finished` seeds only `BtrfsScrubStatus`
   (`cli/src/test_fixtures/idle.rs:207-210`).
2. `MockRunner::dispatch` returns `CmdError::MissingMock` for any
   unseeded request (`cli/src/cmd.rs:1080-1081`), which `cmd_idle` then
   reports as `BusyReason::Unknown` (`cli/src/idle.rs:77-78`).

Either of those can quietly shift -- if the helper is broadened to seed
more requests (e.g. via a shared fixture), or if a future `MockRunner`
change returns a benign default instead of `MissingMock` -- and the
test will silently stop protecting against the very regression its
preamble names.

The fix is to make the guarantee explicit: pin the exact recorded
request vector, so any extra `CmdRequest` issued from inside `cmd_idle`
fails at the assertion line instead of relying on the chain above.

Ordering matters: `MockRunner::run` pushes the request onto the log
*before* it dispatches (`cli/src/cmd.rs:1201-1209`), so the recorded
log holds the regressing call regardless of whether dispatch then
returns `MissingMock`. We therefore want the `runner.requests()`
assertion to fire *first*, naming the offending `CmdRequest` directly,
rather than letting the result assertion fail first with a generic
`Idle != Busy(Unknown(...))` message that only re-establishes the
implicit `MissingMock` chain.

Today only the scrub probe is expected to fire (`cli/src/idle.rs:74-79`
is the only `runner.run(...)` call in `cmd_idle`; see also
`docs/decisions/016-auto-suspend.md:53`), so the expected vector is a
single `BtrfsScrubStatus`. The pattern is already used twice in the
same file by sister tests:

- `busy_unknown_on_scrub_probe_failure` (`cli/src/idle.rs:367-372`)
- `busy_unknown_on_scrub_parse_failure` (`cli/src/idle.rs:399`)

and the broader codebase has 22 uses of `runner.requests()` for the
same purpose, so this aligns the regression test with an established
convention rather than introducing a new pattern.

## Files Modified

- `cli/src/idle.rs` -- single test body change.

## Change

In `no_balance_or_replace_subprocess_calls`
(`cli/src/idle.rs:336-351`), insert a request-vector pinning assertion
**immediately after** the `cmd_idle` call and **before** the existing
result assertion, and rewrite the preamble's "Why" line so the
recorded request log -- not `MissingMock` -- is named as the guard.
`CmdRequest` is already in scope in the test module via `use super::*`
(the parent module imports it at `cli/src/idle.rs:1`).

```rust
// Intent: `cmd_idle` must NOT call `BtrfsBalanceStatus`,
//   `BtrfsReplaceStatus`, or `BtrfsFilesystemShow`.
//   Those subprocess probes were removed in favor of the sysfs scan.
// Why: Pins the contract that the refactor preserves by asserting the
//   exact recorded request log -- the only `CmdRequest` `cmd_idle` may
//   issue is `BtrfsScrubStatus`. Re-introducing any other subprocess
//   probe fails this assertion directly, naming the offending request,
//   independent of how the runner happens to handle unmocked calls.
// Scenario: Future change accidentally re-introduces a subprocess
//   probe; this test catches it before merge.
#[test]
fn no_balance_or_replace_subprocess_calls() {
    let (runner, fs) = idle_ready_for_sysfs_check("none");
    let result = cmd_idle(&runner, &fs, &idle_mp());
    assert_eq!(
        runner.requests(),
        vec![CmdRequest::BtrfsScrubStatus {
            mount_point: idle_mp(),
        }],
    );
    assert_eq!(result, IdleResult::Idle);
}
```

No production code, fixtures, helpers, or other tests change.

## Reused Helpers

- `idle_ready_for_sysfs_check` -- `cli/src/test_fixtures/idle.rs:213`
  (unchanged; supplies the narrow runner that seeds only
  `BtrfsScrubStatus`).
- `idle_mp` -- `cli/src/test_fixtures/idle.rs` (re-exported in the test
  use-list at `cli/src/idle.rs:113`).
- `MockRunner::requests` -- `cli/src/cmd.rs:1104-1109` (already
  the canonical way to read the recorded request log).
- `CmdRequest::BtrfsScrubStatus` -- in scope via the top-of-file
  `use crate::cmd::{CmdRequest, CommandRunner};` at `cli/src/idle.rs:1`.

## Verification

End-to-end check that the assertion both holds today and would catch
the regression it is meant to catch:

1. **Test passes as-is.** Run the affected Rust unit test:

   ```
   just test-rust
   ```

   Specifically `cli::idle::tests::no_balance_or_replace_subprocess_calls`
   should pass.

2. **Manual regression check (do not commit).** Temporarily insert a
   stray `let _ = runner.run(&CmdRequest::BtrfsBalanceStatus {
   mount_point: mount_point.clone() });` near the top of `cmd_idle`
   (`cli/src/idle.rs:45`), then re-run `just test-rust`. The test must
   now fail at the new `assert_eq!(runner.requests(), ...)` line --
   which runs before the result assertion -- with a diff that names
   the extra `BtrfsBalanceStatus` call. The failure must not come from
   the result assertion or from any `MissingMock`-induced `Busy::Unknown`
   message. Revert the experiment.

   (This step is what proves the new assertion is doing the work, not
   the implicit `MissingMock` chain. Because `MockRunner::run` records
   the request before dispatching (`cli/src/cmd.rs:1201-1209`), the
   regressing call appears in `runner.requests()` even when dispatch
   returns `MissingMock`. It is a local sanity check, not a committed
   test.)

3. **No unrelated breakage.** `just test-rust` for the whole crate
   must remain green.

VM tests, fixture refreshes, and parser canaries are not implicated --
the change is contained to one Rust unit test and does not touch any
parser, command surface, or systemd unit.
