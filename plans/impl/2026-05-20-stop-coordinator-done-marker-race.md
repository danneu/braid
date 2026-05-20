# Fix TOCTOU race in `RealStopCoordinator::poll_for_done_or_release`

## Context

`RealStopCoordinator` (`cli/src/pool_lock.rs:221-265`) coordinates plain
`braid lock` with the recursive `braid lock --systemd-stop` ExecStop
reentry. The protocol: plain holds the coordinator flock across
`cmd_lock` -> `mark_done` (writes `done\n`) -> `mark_offline`
(synchronous `systemctl stop`). The reentry polls for either `done\n`
on the file or flock release.

There is a narrow TOCTOU window inside `poll_for_done_or_release`
(`pool_lock.rs:246-264`). The loop reads file content first, then calls
`acquire()`. `acquire()` unconditionally `set_len(0)`s the file after
winning the flock. Sequence that triggers the bug:

1. Reentry's poll iteration: `std::fs::read` returns empty bytes
   (predecessor has not yet written `done\n`).
2. Predecessor calls `mark_done()` -- file becomes `done\n`.
3. Predecessor dies inside `mark_offline()` (signal / OOM / panic in
   the runner subprocess) before its own coordinator guard drops.
   Kernel releases the flock on process death.
4. Reentry's `acquire()` wins the flock and immediately wipes
   `done\n`.
5. Reentry interprets the empty file as "predecessor died before
   completing cleanup" and re-runs `cmd_lock` against the
   already-unmounted pool.

Steady-state consequence is benign: a redundant `cmd_lock` against an
unmounted pool no-ops past the unmount step and only sweeps orphan
mappers. The real cost is the destroyed forensic signal -- after the
fact, an operator inspecting `/run/braid-stop-coordinator.lock` cannot
distinguish "predecessor died before mark_done" from "predecessor died
after mark_done". Aligning the seam with the documented protocol (the
plan doc calls truncate-on-acquire a "defense-in-depth against stale
content from a crashed predecessor") is a small, structural correction.

The truncate-on-acquire is load-bearing for fresh-transition callers
(`run_plain_lock` at `main.rs:987`, `run_systemd_stop_lock`'s first
acquire at `main.rs:1024`). Without it, a stale `done\n` from a
previous session's plain `braid lock` would survive across the next
fresh-transition acquire and cause future reentries to short-circuit
without running cleanup. So the fix must be asymmetric: keep
`acquire()` truncating; teach the poll path to acquire without
truncating and re-read content post-acquire.

## Approach

Extract a private helper `open_and_lock` that does open + non-blocking
flock with no truncate. Make `acquire()` call it then truncate
(externally observable behavior unchanged). Rewrite
`poll_for_done_or_release` to use `open_and_lock` directly, then re-read
the file content. If `done\n`, drop the file (releases flock) and return
`Done` -- the on-disk marker is preserved. Otherwise return `Acquired`
wrapping a guard built from the non-truncated file. The protocol only
ever writes `DONE_MARKER`, so if content post-acquire is not `done\n`,
it is empty; no truncate is required on the `Acquired` branch.

Considered alternatives:

- **Lift truncate to call sites.** Loses the seam's defensive role
  against stale `done\n` from a prior session and duplicates the
  responsibility at every fresh-transition call site. Rejected.
- **`bool truncate` parameter on a single internal method.** Same
  shape, uglier API. Rejected.
- **Conditional truncate inside `acquire()` (preserve when content is
  `done\n`).** Would preserve stale `done\n` across unrelated
  transitions and poison future reentries. Wrong.

## Files to modify

- `cli/src/pool_lock.rs` -- all code and test changes live here.

No production-code changes outside `pool_lock.rs`. `acquire()`'s
public signature and behavior are preserved, so `main.rs:987` and
`main.rs:1024` need no edits.

## Implementation

### 1. Add `open_and_lock` helper

Insert in `impl RealStopCoordinator` immediately before `acquire()`:

```rust
/// Open the coordinator file and take its exclusive flock without
/// touching content. Shared between `acquire()` (which then
/// truncates to claim a clean slate for a fresh transition) and
/// `poll_for_done_or_release` (which re-reads to disambiguate
/// "predecessor died after mark_done" from "predecessor died
/// before mark_done").
fn open_and_lock(&self) -> Result<File, StopCoordinatorError> {
    let file = open_lock_file(&self.path)?;
    match flock(file.as_raw_fd(), FlockArg::LockExclusiveNonblock) {
        Ok(()) => Ok(file),
        Err(e) if would_block(e) => Err(StopCoordinatorError::Held),
        Err(e) => Err(StopCoordinatorError::Io(io_from_errno(e))),
    }
}
```

### 2. Rewrite `acquire()` in terms of the helper

Replace current body (`pool_lock.rs:234-244`):

```rust
pub fn acquire(&self) -> Result<StopCoordinatorGuard, StopCoordinatorError> {
    let file = self.open_and_lock()?;
    file.set_len(0).map_err(StopCoordinatorError::Io)?;
    Ok(StopCoordinatorGuard { file })
}
```

Externally observable behavior unchanged.

### 3. Close the TOCTOU window in `poll_for_done_or_release`

Replace current body (`pool_lock.rs:246-264`) with a thin public
wrapper and a private impl method that takes a hook closure. The hook
runs between the pre-read and the lock attempt, fires once per loop
iteration, and lets a deterministic test inject the race window
(predecessor writes `done\n` and drops its guard between R's pre-read
and R's acquire). In production the wrapper passes a no-op closure;
Rust's generic monomorphization compiles that to zero overhead.

```rust
pub fn poll_for_done_or_release(&self, deadline: Duration) -> StopCoordinatorPollResult {
    self.poll_for_done_or_release_inner(deadline, || {})
}

/// Inner loop, parameterized by a hook that fires after the
/// pre-acquire content read and before the flock attempt on every
/// iteration. Tests use the hook to simulate a predecessor that
/// writes `done\n` and then dies inside the TOCTOU window; the
/// production wrapper passes a no-op.
fn poll_for_done_or_release_inner<F: FnMut()>(
    &self,
    deadline: Duration,
    mut after_pre_read: F,
) -> StopCoordinatorPollResult {
    let start = Instant::now();
    loop {
        // Pre-acquire short-circuit: predecessor wrote `done\n` and
        // still holds the flock. Without this we would block until
        // the predecessor releases or the deadline expires.
        if std::fs::read(&self.path).is_ok_and(|bytes| bytes == DONE_MARKER) {
            return StopCoordinatorPollResult::Done;
        }
        after_pre_read();
        match self.open_and_lock() {
            Ok(file) => {
                // Post-acquire re-read closes the TOCTOU window
                // between the pre-read above and the flock win: the
                // predecessor may have written `done\n` and then
                // died (releasing the flock on process death)
                // between those two points.
                if std::fs::read(&self.path).is_ok_and(|bytes| bytes == DONE_MARKER) {
                    // Dropping `file` releases the flock. On-disk
                    // `done\n` is preserved as the forensic signal
                    // that the predecessor reached mark_done.
                    drop(file);
                    return StopCoordinatorPollResult::Done;
                }
                return StopCoordinatorPollResult::Acquired(StopCoordinatorGuard { file });
            }
            Err(StopCoordinatorError::Held) if start.elapsed() < deadline => {
                thread::sleep(STOP_COORDINATOR_POLL_INTERVAL);
            }
            Err(StopCoordinatorError::Held) => return StopCoordinatorPollResult::Deadline,
            Err(_) if start.elapsed() < deadline => {
                thread::sleep(STOP_COORDINATOR_POLL_INTERVAL);
            }
            Err(_) => return StopCoordinatorPollResult::Deadline,
        }
    }
}
```

Notes on correctness:

- Post-acquire `std::fs::read` failure: `is_ok_and` short-circuits to
  `false`, the function returns `Acquired(guard)`. Same fallback shape
  as the existing pre-read.
- `File::drop` closes the fd, which releases the BSD flock. Explicit
  `drop(file)` before returning `Done` makes the release point
  obvious.
- Reading via path while holding the flock is safe: the only writer
  is a flock holder, and we are it.
- `StopCoordinatorGuard { file }` from a non-truncated file is fine.
  The guard's only method, `mark_done`, does `set_len(0)` before
  writing, so the guard restores its own empty-content invariant.
- The hook is a private seam, not a public extension point: `_inner`
  is not `pub`, only the same-file `tests` submodule can reach it via
  `super::*`. The no-op closure compiles away in the production
  call. Both call sites (production wrapper, deterministic test)
  monomorphize to one instance each -- no code-bloat concern.

## Tests

Add two new unit tests to `#[cfg(test)] mod tests` in `pool_lock.rs`.
Use the project's three-section `// Intent / Why it exists / Scenario`
preamble convention (per `AGENTS.md`) -- the existing five
`stop_coordinator_*` tests in the same module are bare, but new tests
should follow the project standard. Do not retrofit preambles onto the
existing five.

### Test A: `open_and_lock_preserves_pre_seeded_done`

Structural test of the new helper.

```rust
// Intent: open_and_lock takes the flock without touching file
// content.
// Why it exists: poll_for_done_or_release depends on the post-acquire
// re-read to disambiguate "predecessor died after mark_done" from
// "predecessor died before mark_done". A refactor that re-introduces
// truncate-on-acquire into this helper would silently reintroduce the
// redundant-cmd_lock race. acquire() truncates only because it is
// reserved for fresh-transition callers where pre-existing content is
// stale.
// Scenario: a prior session wrote done\n then exited; this session's
// poll path calls open_and_lock as part of disambiguating predecessor
// state.
#[test]
fn open_and_lock_preserves_pre_seeded_done() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("coord.lock");
    std::fs::write(&path, DONE_MARKER).unwrap();
    let coord = RealStopCoordinator::new(&path);
    let file = coord.open_and_lock().expect("flock should succeed on fresh file");
    drop(file);
    assert_eq!(std::fs::read(&path).unwrap(), DONE_MARKER);
}
```

### Test B: `poll_for_done_or_release_returns_done_when_predecessor_marks_done_and_dies_between_pre_read_and_acquire`

Deterministic regression test for the TOCTOU race itself. Uses the
private `after_pre_read` hook from `poll_for_done_or_release_inner` to
simulate the predecessor's mark_done + death landing in the window
between the poller's pre-read and its flock attempt. Without this
test, the actual ADR 026 contract ("release without `done\n` runs
cleanup, `done\n` exits") is unguarded for the post-acquire branch the
fix introduces -- a future refactor that re-truncated on acquire
would still pass Test A and every existing test.

```rust
// Intent: poll_for_done_or_release returns Done and preserves the
// on-disk done\n marker when the predecessor wrote done\n and died
// in the window between the poller's pre-read and the poller's flock
// attempt.
// Why it exists: this is the specific TOCTOU race the fix closes.
// The pre-read short-circuit cannot see the marker (predecessor has
// not written it yet at pre-read time); the bug is whether the
// post-acquire branch re-reads and observes the marker that the
// predecessor wrote in between. A regression that re-introduced
// truncate-on-acquire would silently wipe the marker on the
// post-acquire branch and reduce this test's expected Done to
// Acquired.
// Scenario: plain `braid lock` is in cmd_lock at the moment the
// reentry's first poll iteration fires. Plain finishes cmd_lock,
// writes done\n via mark_done, and is then SIGKILL'd inside
// mark_offline (before its coordinator guard drops naturally). The
// kernel releases the flock on process death. The reentry's next
// open_and_lock wins the flock and must observe the surviving
// done\n on the post-acquire re-read.
#[test]
fn poll_for_done_or_release_returns_done_when_predecessor_marks_done_and_dies_between_pre_read_and_acquire() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("coord.lock");
    let coord = RealStopCoordinator::new(&path);

    // Predecessor holds the flock at the moment the poller's
    // pre-read fires (file is empty because acquire() truncated).
    let predecessor = coord.acquire().expect("predecessor wins flock");
    let predecessor_cell = std::cell::Cell::new(Some(predecessor));

    let result = coord.poll_for_done_or_release_inner(
        Duration::from_millis(100),
        || {
            // Between pre-read and acquire: predecessor writes done\n
            // and dies (drops its guard, releasing the flock).
            if let Some(p) = predecessor_cell.take() {
                p.mark_done().expect("mark_done writes DONE_MARKER");
                drop(p);
            }
        },
    );

    assert!(
        matches!(result, StopCoordinatorPollResult::Done),
        "expected Done after predecessor wrote done\\n and died in the TOCTOU window"
    );
    assert_eq!(
        std::fs::read(&path).unwrap(),
        DONE_MARKER,
        "post-acquire branch must preserve the on-disk done\\n marker"
    );
}
```

### Existing tests that must keep passing

All five existing `stop_coordinator_*` tests
(`pool_lock.rs:357-422`):

- `stop_coordinator_acquire_then_second_acquire_returns_held` --
  flock contention still routes through `open_and_lock`.
- `stop_coordinator_acquire_truncates_stale_done` -- `acquire()`
  still truncates.
- `stop_coordinator_poll_returns_done_while_holder_still_holds` --
  pre-read short-circuit retained.
- `stop_coordinator_poll_returns_acquired_after_holder_releases_without_done`
  -- post-acquire re-read of empty file returns `Acquired`.
- `stop_coordinator_poll_returns_deadline_when_held_with_empty_content`
  -- `open_and_lock` returns `Held`, loop deadlines.

VM-level test `tests/module/braid-lock-coordinator-race.py` exercises
the happy-path race (predecessor does not die) and is unaffected.

## Verification

From repo root:

1. `just test-rust` -- runs the lib unit tests. Must include all
   five existing `stop_coordinator_*` cases plus the two new tests
   (`open_and_lock_preserves_pre_seeded_done`,
   `poll_for_done_or_release_returns_done_when_predecessor_marks_done_and_dies_between_pre_read_and_acquire`).
   Spot-check the deterministic race test runs in single-digit
   milliseconds -- if it ever flakes, the hook path is broken.
2. `just test-vm braid-lock-coordinator-race` -- happy-path race
   still passes; the fix only changes behavior on the dead-plain
   path that the VM test does not exercise.

Optional sanity checks:

- `cargo clippy --workspace --all-targets -- -D warnings` -- new
  helpers are private, no doc-on-public-item lint.

## Out of scope

- Sibling lock-coordinator file-content protocols: none exist in the
  codebase (`grep -rln set_len.0.\|StopCoordinator` over `cli/src`
  confirms `pool_lock.rs` is unique). No alignment work.
- Retrofitting three-section preambles onto the existing five
  `stop_coordinator_*` tests.
- The analogous concern at `run_systemd_stop_lock`'s first acquire
  (`main.rs:1024`): if a stale `done\n` survives across sessions, the
  current truncate-on-acquire is what prevents the new fresh
  transition from short-circuiting on it. Preserving that truncate is
  intentional, not a follow-up.
