# Plan: port pool_lock to `nix::fcntl::Flock<T>`

## Context

`cli/src/pool_lock.rs` uses `nix::fcntl::flock`, the free function the
nix crate marked `#[deprecated(since = "0.28.0", note = "fcntl::Flock
should be used instead.")]` (see `reference/nix-crate/src/fcntl.rs:1003`).
braid pins `nix = "0.31.3"` in `cli/Cargo.toml`, so the deprecation
applies; the module silences it with a module-level
`#![allow(deprecated)]` at `cli/src/pool_lock.rs:7`.

The current code works -- the kernel releases the BSD flock when the
last fd to the open file description closes, and the guard structs
hold a `File` for that purpose. But three things are unsatisfying:

1. The next `nix` bump that removes the deprecated symbol will break
   compilation.
2. The module-level allow would silently mask any other deprecated
   `nix` API that landed in this module later. (Today it covers nothing
   else -- see "Evidence" below -- but the surface is wider than it
   needs to be.)
3. The header comment says "deliberately uses BSD flock(2) via nix",
   which is misleading: `Flock<T>` is also BSD `flock(2)`. The
   "deliberate" justification doesn't actually pick the deprecated
   API.

The replacement, `nix::fcntl::Flock<T: Flockable>`
(`reference/nix-crate/src/fcntl.rs:1038`), wraps the locked fd,
implements `Deref<Target = T>`, and explicitly issues `LOCK_UN` on
`Drop`. `Flockable` is implemented for both `std::fs::File` and
`OwnedFd`, so the existing `File`-typed guard fields translate
one-for-one. No feature-gate change is needed; `Flock<T>` is
unconditional on Linux targets and lives in the already-enabled `fs`
surface.

## Scope

Single file: `cli/src/pool_lock.rs`. No other call sites change:

- All four external acquisition sites in `cli/src/main.rs:1046`,
  `:1058`, `:1071`, `:1230` bind the returned guard for `Drop`
  semantics only; no field reads.
- `cli/src/lock.rs:996` takes `coordinator_guard:
  &StopCoordinatorGuard` and calls `.mark_done()` (lines 1010, 1031,
  1259). That method stays.
- No test reaches into guard fields. Tests that hold the value
  returned from `open_and_lock()` (line 400-403) just `drop(file)` --
  works identically for `Flock<File>`.

## Changes

All edits inside `cli/src/pool_lock.rs`:

1. **Imports / allow.** Replace
   `use nix::fcntl::{FlockArg, OFlag, flock, open};` with
   `use nix::fcntl::{Flock, FlockArg, OFlag, open};` and delete the
   module-level `#![allow(deprecated)]` plus its comment.

2. **Guard inner types.** Change `RealPoolLockGuard._file: File` and
   `StopCoordinatorGuard.file: File` to hold `Flock<File>` instead. The
   `RealPoolLockGuard` field stays underscore-prefixed (drop-only).
   For `StopCoordinatorGuard`, rename `file` -> `lock` so the field
   name reflects what it owns; `mark_done` calls `self.lock.set_len(0)`
   / `self.lock.write_all_at(...)` which compile via
   `Flock<File>: Deref<Target = File>`.

3. **`RealPoolLock::try_acquire`** (cli/src/pool_lock.rs:76-83).
   Replace
   ```
   let file = open_lock_file(&self.path)?;
   match flock(file.as_raw_fd(), FlockArg::LockExclusiveNonblock) { ... }
   ```
   with
   ```
   let file = open_lock_file(&self.path)?;
   match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
       Ok(lock) => Ok(RealPoolLockGuard { _lock: lock }),
       Err((_, e)) if would_block(e) => Err(PoolLockError::AlreadyHeld),
       Err((_, e)) => Err(PoolLockError::Io(io_from_errno(e))),
   }
   ```
   The `(T, Errno)` error tuple discards the returned `File`; we don't
   need it back since the next call re-opens.

4. **`RealStopCoordinator::open_and_lock`** (cli/src/pool_lock.rs:140-147).
   Same shape -- return `Result<Flock<File>, StopCoordinatorError>` and
   match `Flock::lock(...)`.

5. **`RealStopCoordinator::acquire`** (lines 149-153). The
   `file.set_len(0)` call survives unchanged because `Flock<File>`
   derefs to `File`. Store the returned `Flock<File>` in
   `StopCoordinatorGuard.lock`.

6. **`RealStopCoordinator::poll_for_done_or_release_inner`** (lines
   164-189). The `Ok(file) => { ... drop(file); ... }` and
   `StopCoordinatorGuard { file }` arms type-check unchanged once
   `open_and_lock` returns `Flock<File>` and the guard field is renamed.

7. **Drop the `AsRawFd` import.** After this change nothing in the
   module calls `as_raw_fd()` directly -- `Flock<T>` does that
   internally. Remove `use std::os::fd::AsRawFd;`.

## Behavioral notes

- The kernel syscall is identical: `libc::flock(fd, LOCK_EX | LOCK_NB)`
  for acquire and `libc::flock(fd, LOCK_UN)` for release.
- Cross-process release on holder death is a kernel guarantee that
  holds for both APIs -- nothing in the systemd-stop / SIGKILL path
  changes.
- `Flock<T>::drop` (`reference/nix-crate/src/fcntl.rs:1042-1049`)
  panics if `LOCK_UN` fails and the thread isn't already panicking.
  On Linux this can only fail with `ENOLCK` (kernel out of lock-record
  memory) -- vanishingly rare and worse failure modes would already be
  in flight if we hit it. Not a behavior regression worth working
  around.

## Verification

1. **Compile.** `cargo check -p braid-cli` from the repo root. The
   deprecation warning that motivated the `#![allow(deprecated)]`
   should be gone; the build should warn-free for that symbol.
2. **Unit tests.** `just test-rust`. The existing tests in
   `cli/src/pool_lock.rs:224-495` already exercise:
   - non-blocking contention (`acquire_returns_already_held_on_second_attempt`),
   - bounded-wait success after release
     (`acquire_with_timeout_polls_then_succeeds_after_holder_release`,
     `acquire_with_systemd_stop_deadline_polls_then_succeeds_after_holder_release`),
   - the stop-coordinator TOCTOU window
     (`poll_for_done_or_release_returns_done_when_predecessor_marks_done_and_dies_between_pre_read_and_acquire`),
   - `open_and_lock_preserves_pre_seeded_done`,
   - `stop_coordinator_acquire_truncates_stale_done`.
   No new tests required -- the change is API-only; behavior is
   structurally identical.
3. **VM tests.** `just test-vm` to confirm the systemd-stop deadline
   and braid-online lifecycle paths still pass end-to-end. The lock
   ownership semantics they cover are unchanged.

## Out of scope

- No change to `lock_policy` in `cli/src/main.rs`, the lock-path
  selection, or any consumer of `RealPoolLockGuard` /
  `StopCoordinatorGuard`.
- No `Cargo.toml` change. `Flock<T>` and `Flockable` are not
  feature-gated in `nix` 0.31.
- No ADR 026 update needed. The decision is about *where* the lock
  lives in the dispatch flow, not the Rust binding choice.
