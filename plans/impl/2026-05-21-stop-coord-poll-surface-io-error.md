# Plan: stop swallowing I/O errors in stop-coordinator poll

## Context

`RealStopCoordinator::poll_for_done_or_release_inner`
(`cli/src/pool_lock.rs:157-186`) has two `Err(_)` arms that absorb any
non-`Held` error from `open_and_lock` -- including `EACCES`, `ENOSPC`,
or any other failure to create/open `/run/braid-stop-coordinator.lock`
-- and silently keep polling until the deadline, then return
`StopCoordinatorPollResult::Deadline`. The caller
(`run_systemd_stop_lock` at `cli/src/main.rs:1185-1192`) then prints
`braid: pool lock not released within ... -- aborting --systemd-stop`
to the journal, which is wrong: there was no contention; the operator's
actual fault is an unwritable coordinator path. The same parser routes
`replace`'s contention messaging through this module, so the diagnosis
surface is shared even though the failing path is `--systemd-stop`-only.

The sibling polling helper in the same file,
`RealPoolLock::poll_acquire` (`cli/src/pool_lock.rs:82-98`), already
handles this correctly: it loops only on `AlreadyHeld` and uses
`Err(e) => return Err(e)` to short-circuit any I/O error to the caller.
The stop-coordinator path diverged from that pattern at introduction
(`ff6f766`) and was not corrected by the subsequent done-marker fix
(`e1d1e94`). The ideal fix aligns the two helpers so future maintainers
do not have to remember two divergent poll-loop conventions, and so
operators see the real I/O error instead of a fabricated deadline.

## Approach

Change `poll_for_done_or_release` (and `_inner`) to return
`Result<StopCoordinatorPollResult, StopCoordinatorError>`. Collapse the
two `Err(_)` arms into a single short-circuit:
`Err(e) => return Err(e)`. The `Held` retry-until-deadline branch and
the `Held -> Deadline` expiry branch are unchanged. The poll result
enum keeps its existing three variants -- `Done`, `Acquired`, `Deadline`
-- with no new `Io` variant, because I/O is already first-class on the
function's error channel via `StopCoordinatorError::Io`.

The caller already distinguishes
`Err(StopCoordinatorError::Io(e)) => print_cli_error(&e.to_string())`
on the initial `acquire()` at `main.rs:1194-1197`. The poll's new
`Err(StopCoordinatorError::Io(e))` path reuses the same handler --
collapse the nested match into a single flow that funnels both the
initial acquire's Io error and the polled Io error through one
`print_cli_error` exit.

Rationale for not introducing `StopCoordinatorPollResult::Io(io::Error)`:
that variant would duplicate the existing `StopCoordinatorError::Io`
concept and prevent the result enum from describing only the three
real success/contention outcomes. Returning `Result<..., ...>` keeps
the error channel orthogonal to the contention-outcome channel, which
is the shape `poll_acquire` already uses for `RealPoolLock`.

## Files to modify

- `cli/src/pool_lock.rs`
  - `poll_for_done_or_release` (line 152) and
    `poll_for_done_or_release_inner` (line 157): return
    `Result<StopCoordinatorPollResult, StopCoordinatorError>`. Replace
    the two `Err(_)` arms at lines 180-183 with a single
    `Err(e) => return Err(e)`. The existing `Held` arms at lines 176
    and 179 are untouched. The success arms at lines 165, 172, 174 now
    return `Ok(...)`.
  - Existing tests at lines 430, 457, 473, 487 use the result enum
    directly; update them to pattern-match on `Ok(...)` (or use
    `.expect("poll succeeded")` then match). The TOCTOU test at line
    421 uses `poll_for_done_or_release_inner` and is the most involved
    update.
  - Add one new test:
    `stop_coordinator_poll_returns_err_io_when_coordinator_path_is_unwritable`.
    Construct a `RealStopCoordinator` with a path under a non-existent
    parent directory (e.g. `dir.path().join("missing/coord.lock")`) so
    `open_lock_file` fails with `ENOENT` on first iteration. Assert
    that the poll returns `Err(StopCoordinatorError::Io(_))` rather
    than `Ok(StopCoordinatorPollResult::Deadline)`. Include the
    three-section preamble (Intent / Why it exists / Scenario) per
    project test convention.

- `cli/src/main.rs`
  - `run_systemd_stop_lock` (line 1174): the inner match at line
    1185-1192 now binds a `Result`. Flatten it so a polled
    `StopCoordinatorError::Io(e)` is handled by the same
    `print_cli_error(&e.to_string()); std::process::exit(1)` arm that
    today only catches the initial `acquire()`'s Io error
    (lines 1194-1197). The simplest shape is to convert the outer
    `match stop_coordinator.acquire()` so its `Err(Held)` branch
    evaluates the poll result and unifies error handling at the outer
    level; or to add a sibling `Err(StopCoordinatorError::Io(e))` arm
    on the polled result. Either is fine -- prefer whichever produces
    the smaller diff and avoids duplicating the I/O error-printing
    arm.

## What is intentionally not changing

- The `Held` retry cadence and the `Held -> Deadline` expiry behavior
  are unchanged. Existing VM coverage of the `--systemd-stop`
  contention path stays valid.
- The Done-marker TOCTOU semantics (pre-read short-circuit, post-acquire
  re-read, `open_and_lock`'s no-truncate contract) are unchanged. The
  fix touches only the error-handling arms of the match, not the
  loop's done-detection logic.
- `RealPoolLock::poll_acquire` is unchanged; the fix aligns the stop
  coordinator with it, not the other way around.
- No public API outside the `pool_lock` module changes -- only the
  stop-coordinator helper's return type and `run_systemd_stop_lock`'s
  match shape.

## Verification

- `just test-rust` -- runs the updated existing tests plus the new
  unwritable-path test. The existing tests confirm the
  `Done` / `Acquired` / `Deadline` paths still work behind the new
  `Result` wrapper; the new test confirms an I/O error now
  short-circuits to `Err(Io(_))` instead of masquerading as
  `Ok(Deadline)`.
- `cargo check -p braid-cli` (implicitly part of `just test-rust`) --
  catches any forgotten call-site adjustments.
- No new VM test required. The systemd-stop path's contention behavior
  is unchanged and is already covered by the existing VM suite;
  triggering a real coordinator I/O error in a VM would require either
  filling `/run` (out of scope) or remounting it read-only mid-stop
  (a non-realistic operator scenario for a regression test). The
  unit test exercises the failure mode deterministically.
