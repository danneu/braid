# Plan: close the run_plain_lock test gap

## Context

`plans/impl/2026-05-19-rust-owned-pool-operation-lock.md` was promoted
to `plans/impl/` in commit `ff6f766` (`fix(lock): move pool lock
ownership into rust dispatch`, 2026-05-19). That commit landed the new
plain-lock orchestration in `cli/src/main.rs::run_plain_lock`
(`cli/src/main.rs:981-1014`) plus eight new VM tests, but it skipped
the two Rust unit tests that the plan itself prescribed in §step 3.3
(`plans/impl/2026-05-19-rust-owned-pool-operation-lock.md:793-840`).
Pickaxe (`git log -S 'coord_file_path'`,
`-S 'cmd_lock_failure_does_not'`, `-S 'cmd_lock_success_writes'`)
returns only that single commit, with all hits inside the plan-file
text. No follow-up implementation exists.

The missing tests are the regression gate for a load-bearing safety
invariant: in `run_plain_lock`, `cmd_lock` must succeed (early-return
on `Err`) BEFORE `coordinator_guard.mark_done()` writes `done\n` to
the stop-coordinator file, and BEFORE `mark_offline(...)` issues
`systemctl stop braid-online.service`. A regression that reorders
`mark_done` ahead of the `cmd_lock?` check -- or that calls
`mark_offline` unconditionally -- would let a concurrent ExecStop
reentry observe `done\n` while the pool is still mounted with open
mappers, exit 0, and silently mark the unit inactive over a still-
online pool. None of the existing VM tests catch this shape:
`braid-lock-then-unlock-no-race.py:28-31` and
`braid-lock-coordinator-race.py:31-47` exercise only success paths;
`execstop-cleans-stale-online.py:24-38` exercises an out-of-band
unmount, not a plain-lock failure mid-flight.

`run_plain_lock` today is also unreachable from unit tests: it takes
concrete `&RealPoolLock`, `&RealStopCoordinator`, inlines `RealRunner`
/ `RealFilesystem` / `RealOnlineStateOps::new(&runner)`, and calls
`std::process::exit(1)` on every error path. Closing the test gap
requires extracting the orchestration into a testable function and
adding a small instrumentation hook to `RecordingOnlineStateOps`.

## Goal

Land the two unit tests the plan prescribed (plus a symmetric third
test for `mark_done` IO failure), structurally guarded by the smallest
possible refactor. End state:

1. `cli/src/lock.rs` exposes `pub fn cmd_lock_orchestrate(...)`
   wrapping a private `cmd_lock_orchestrate_impl(...)` with two
   closure-injected seams (`cmd_lock_fn`, `mark_done_fn`). This
   mirrors the existing `cmd_lock` / `cmd_lock_impl` split at
   `cli/src/lock.rs:959-993`.
2. `cli/src/main.rs::run_plain_lock` shrinks to a thin caller: acquire
   guards, load config/membership, build runner/fs/online-ops, call
   `cmd_lock_orchestrate(...)`, translate the typed error to the
   existing `print_cli_error` + `process::exit(1)` shape.
3. `cli/src/online_state.rs::RecordingOnlineStateOps` gains an
   optional `coord_file_path` configuration and an index-aligned
   `coord_snapshots: RefCell<Vec<Vec<u8>>>` log; its `systemctl_stop`
   appends the file's bytes alongside the existing `calls.push(...)`.
4. Three unit tests in `cli/src/lock.rs`'s existing `mod tests` cover
   the failure, success, and `mark_done`-failure paths.

Production behavior is unchanged. The orchestrator is a structural
extraction; no error semantics, ordering, or runtime side effects
change.

## Critical files

- `cli/src/main.rs:981-1014` -- `run_plain_lock` (shrinks to thin
  caller).
- `cli/src/lock.rs:959-993` -- existing `cmd_lock` / `cmd_lock_impl`
  pattern to mirror; add new orchestrator below it.
- `cli/src/online_state.rs:91-99` -- `OnlineStateOps` trait
  (re-used).
- `cli/src/online_state.rs:292-311` -- `mark_offline` (re-used as-is).
- `cli/src/online_state.rs:314-388` -- `RecordingOnlineStateOps`
  (extended).
- `cli/src/pool_lock.rs:268-278` -- `StopCoordinatorGuard::mark_done`
  (re-used as-is; same-process readers see `done\n` reliably -- no
  fsync needed, no behavior change).
- `cli/src/test_fixtures/lock.rs` -- existing helpers
  (`lock_test_membership()`) reused where useful; the three new tests
  mostly need a `tempfile::tempdir()` + a stub `MockRunner`/`MockFs`
  because both seams (`cmd_lock_fn`, `mark_done_fn`) are stubbed via
  closures.
- `plans/impl/2026-05-19-rust-owned-pool-operation-lock.md:793-840`
  -- prescriptive source; the implementation here completes its §3.3
  requirement.

## Design

### 1. New error type in `cli/src/lock.rs`

```rust
#[derive(Debug)]
pub enum LockOrchestrateError {
    CmdLock(LockError),
    MarkDone(io::Error),
}

impl std::fmt::Display for LockOrchestrateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CmdLock(e) => write!(f, "{e}"),
            Self::MarkDone(e) => write!(f, "failed to mark lock cleanup done: {e}"),
        }
    }
}
```

`MarkDone`'s `Display` reproduces the exact string `main.rs:1010`
currently prints, so user-visible output is unchanged. No
`std::error::Error` impl, no `thiserror`, no `From` impls -- the
call site uses `Display` only.

### 2. New orchestrator in `cli/src/lock.rs`, alongside `cmd_lock`

```rust
/// Plain-lock orchestration: run cmd_lock, then atomically mark the
/// stop coordinator done, then best-effort deactivate
/// braid-online.service. The ordering -- cmd_lock first, mark_done
/// second, mark_offline last -- is the load-bearing invariant the
/// ExecStop reentry relies on.
pub fn cmd_lock_orchestrate<R, F, O>(
    runner: &R,
    fs: &F,
    online_ops: &O,
    config: &Config,
    membership: &PoolMembership,
    coordinator_guard: &StopCoordinatorGuard,
) -> Result<(), LockOrchestrateError>
where
    R: CommandRunner,
    F: Filesystem + ?Sized,
    O: OnlineStateOps,
{
    cmd_lock_orchestrate_impl(
        runner,
        fs,
        online_ops,
        config,
        membership,
        |r, fs, cfg, mem, dry| cmd_lock(r, fs, cfg, mem, dry),
        || coordinator_guard.mark_done(),
    )
}

fn cmd_lock_orchestrate_impl<R, F, O, CL, MD>(
    runner: &R,
    fs: &F,
    online_ops: &O,
    config: &Config,
    membership: &PoolMembership,
    cmd_lock_fn: CL,
    mark_done_fn: MD,
) -> Result<(), LockOrchestrateError>
where
    R: CommandRunner,
    F: Filesystem + ?Sized,
    O: OnlineStateOps,
    CL: FnOnce(&R, &F, &Config, &PoolMembership, bool) -> Result<(), LockError>,
    MD: FnOnce() -> io::Result<()>,
{
    cmd_lock_fn(runner, fs, config, membership, false)
        .map_err(LockOrchestrateError::CmdLock)?;
    mark_done_fn().map_err(LockOrchestrateError::MarkDone)?;
    let _ = mark_offline(config, online_ops);
    Ok(())
}
```

Imports added to `cli/src/lock.rs`: `crate::pool_lock::
StopCoordinatorGuard`, `crate::online_state::{OnlineStateOps,
mark_offline}`, `std::io`.

This pattern intentionally mirrors `cmd_lock` / `cmd_lock_impl`
(`cli/src/lock.rs:959-993`). The public wrapper hardcodes the real
implementations of both seams; the private impl exposes them for
tests. Tests reach `cmd_lock_orchestrate_impl` from the same crate's
`mod tests`, just like every existing test in `cli/src/lock.rs`
calls `cmd_lock_impl` directly.

`O` is `Sized` (no `?Sized`) on purpose: the body coerces `&O` to
`&dyn OnlineStateOps` at the `mark_offline(config, online_ops)` call,
which requires `O: Sized` for the unsize coercion. `F` keeps
`+ ?Sized` because `cmd_lock_impl` already accepts `&F: ?Sized` and we
pass it straight through. Both real call sites
(`RealOnlineStateOps`, `RecordingOnlineStateOps`) are concrete sized
types, so the tighter bound on `O` costs nothing.

### 3. Thin `run_plain_lock` in `cli/src/main.rs`

`cli/src/main.rs:981-1014` becomes:

```rust
fn run_plain_lock(
    pool_lock: &RealPoolLock,
    stop_coordinator: &RealStopCoordinator,
    config_path: &Path,
    paths: &StatePaths,
) {
    let coordinator_guard = match stop_coordinator.acquire() {
        Ok(guard) => guard,
        Err(StopCoordinatorError::Held) => {
            eprintln!("{}", PoolLockError::AlreadyHeld);
            std::process::exit(1);
        }
        Err(StopCoordinatorError::Io(e)) => {
            print_cli_error(&e.to_string());
            std::process::exit(1);
        }
    };
    let _pool_guard = acquire_pool_or_exit(pool_lock);
    let config = load_config_or_exit(config_path, 1);
    let membership = load_membership_or_exit(paths, 1);
    let runner = RealRunner;
    let fs = RealFilesystem;
    let online_ops = RealOnlineStateOps::new(&runner);

    if let Err(e) = braid_cli::lock::cmd_lock_orchestrate(
        &runner,
        &fs,
        &online_ops,
        &config,
        &membership,
        &coordinator_guard,
    ) {
        print_cli_error(&e.to_string());
        std::process::exit(1);
    }
}
```

The two side effects (`mark_done`, `mark_offline`) move into the
orchestrator. The single `print_cli_error` + `exit(1)` translation
replaces today's two separate `if let Err` blocks because
`LockOrchestrateError::Display` already produces the right string
for both variants. `run_systemd_stop_lock` (`cli/src/main.rs:1016`)
is untouched -- its arm does not write `done\n` or call
`mark_offline` (it polls the coordinator file as the ExecStop
reentry), so the orchestration concern does not apply there.

### 4. Extend `RecordingOnlineStateOps` in `cli/src/online_state.rs`

Add two fields to the struct at `cli/src/online_state.rs:314-319`:

```rust
pub struct RecordingOnlineStateOps {
    state: std::cell::RefCell<Result<UnitActiveState, String>>,
    mounted: std::cell::Cell<bool>,
    calls: std::cell::RefCell<Vec<String>>,
    bound_by: std::cell::RefCell<Result<Vec<String>, String>>,
    coord_file_path: Option<std::path::PathBuf>,
    coord_snapshots: std::cell::RefCell<Vec<Vec<u8>>>,
}
```

`new()` defaults `coord_file_path = None` and an empty
`coord_snapshots`. Add a builder-style configurator:

```rust
pub fn with_coord_file(mut self, path: std::path::PathBuf) -> Self {
    self.coord_file_path = Some(path);
    self
}

pub fn coord_snapshots(&self) -> Vec<Vec<u8>> {
    self.coord_snapshots.borrow().clone()
}
```

Extend `systemctl_stop` at `cli/src/online_state.rs:373-377` (no
change to the existing `calls.push`; append the snapshot only when
`coord_file_path` is set AND the unit matches `BRAID_ONLINE_UNIT`,
so the recorder remains scoped to the orchestrator's stop):

```rust
fn systemctl_stop(&self, unit: &str, no_block: bool) -> Result<(), OnlineError> {
    self.calls
        .borrow_mut()
        .push(format!("stop {unit} no_block={no_block}"));
    if unit == BRAID_ONLINE_UNIT
        && let Some(path) = &self.coord_file_path
    {
        let bytes = std::fs::read(path).unwrap_or_default();
        self.coord_snapshots.borrow_mut().push(bytes);
    }
    Ok(())
}
```

`unwrap_or_default()` is defensive: if any test path probes pre-
`acquire()`, the coordinator file may not yet exist. The two
plan-prescribed tests run after `acquire()`, so this is theoretical.

### 5. Three unit tests in `cli/src/lock.rs`'s existing `mod tests`

All three call `cmd_lock_orchestrate_impl` directly with stub
closures. None of them need full `MockRunner`/`MockFs` setup because
both seams (`cmd_lock_fn`, `mark_done_fn`) are stubbed -- the
`runner` and `fs` parameters are passed straight through to the
closure and never used.

**Common setup for all three tests.** `mark_offline` short-circuits
when the mountpoint is still mounted (`cli/src/online_state.rs:294-
296`) AND skips `systemctl_stop` when `cfg.systemd_lifecycle()` is
false (`cli/src/online_state.rs:305-309`). The default
`RecordingOnlineStateOps::new()` starts `mounted = true` (line
326) and the default `Config` has `systemd_lifecycle = false`, so a
naive setup would make `mark_offline` a no-op -- the success test
would never record a `stop braid-online.service` call, and the two
failure tests' "no stop call" assertions would be vacuous (no stop
would have happened even without the early-return).

Every test must therefore:

1. Parse the config from
   `r#"{"mount_point":"/mnt/storage","systemd_lifecycle":true}"#`
   (this matches the existing pattern in
   `mark_offline_stops_when_lifecycle_enabled` at
   `cli/src/online_state.rs:549-558`).
2. Call `ops.set_mounted(false)` before invoking
   `cmd_lock_orchestrate_impl`, so `is_mountpoint` returns false and
   `mark_offline` proceeds past the early-return into the
   `systemctl_stop` call.

With that setup, `mark_offline` actually fires
`systemctl_stop(BRAID_ONLINE_UNIT, false)` -- which is what the
recorder observes (and snapshots, in the success test).

#### a. `cmd_lock_failure_does_not_write_done_or_stop_online`

Intent (preamble per `docs/testing.md`): orchestrator must not
advance past a failed `cmd_lock`. Stubs `cmd_lock_fn` to return
`Err(LockError::...)` and `mark_done_fn` to a closure that panics if
called (so the test fails if a regression reorders `mark_done` ahead
of the `cmd_lock?` check). Asserts:

- Result is `Err(LockOrchestrateError::CmdLock(_))`.
- `RecordingOnlineStateOps::calls()` contains no
  `"stop braid-online.service no_block=false"` entry.
- `RecordingOnlineStateOps::coord_snapshots()` is empty.
- The coordinator file on disk is empty
  (`std::fs::read(tempdir.path().join("coord")).unwrap().is_empty()`).
  Note: the test owns a `RealStopCoordinator` and calls
  `acquire()` so the file exists; the closure panics if `mark_done`
  is reached, but `set_len(0)` runs at acquire-time, leaving the
  file empty.

#### b. `cmd_lock_success_writes_done_then_calls_mark_offline_in_order`

Intent: orchestrator must `mark_done` BEFORE `mark_offline`. Stubs
`cmd_lock_fn` to `Ok(())`. `mark_done_fn` calls the real
`coordinator_guard.mark_done()` against a `RealStopCoordinator`
rooted at `tempdir.path().join("coord")`. `RecordingOnlineStateOps`
is configured with `with_coord_file(tempdir.path().join("coord"))`.
Asserts:

- Result is `Ok(())`.
- `calls()` contains exactly one
  `"stop braid-online.service no_block=false"` entry.
- `coord_snapshots()` has one entry, equal to `b"done\n"`.
  A regression that swaps the `mark_done` / `mark_offline` order
  yields an empty snapshot, failing the test.
- Defensive post-return check: the file content is `b"done\n"`.

#### c. `mark_done_failure_does_not_call_mark_offline`

Intent: orchestrator must not advance past a failed `mark_done`.
Stubs `cmd_lock_fn` to `Ok(())`. Stubs `mark_done_fn` to
`Err(io::Error::new(io::ErrorKind::Other, "synthetic mark_done failure"))`.
Asserts:

- Result is `Err(LockOrchestrateError::MarkDone(_))`.
- `calls()` contains no `"stop braid-online.service ..."` entry.
- `coord_snapshots()` is empty.

This test extends the plan's prescription by one symmetric case. It
costs no new seam beyond what is already needed for the other two
(both `cmd_lock_fn` and `mark_done_fn` are required to write test
(a) and (b) respectively; test (c) just combines them).

### Doc comments (per AGENTS.md)

- `cmd_lock_orchestrate`: short `///` explaining the ordering
  invariant ("cmd_lock first, mark_done second, mark_offline last
  -- the ExecStop reentry relies on this").
- `LockOrchestrateError`: short `///` explaining why the two
  variants exist as one type ("preserves cmd_lock's typed error
  while distinguishing it from the io::Error path of mark_done").
- No `///` on the private `cmd_lock_orchestrate_impl` (it is the
  pair to the public function -- existing convention in
  `cli/src/lock.rs:969`).
- `with_coord_file`: short `///` ("opt-in instrumentation for the
  cmd_lock_orchestrate tests; snapshots the coordinator file's bytes
  on every systemctl_stop call to BRAID_ONLINE_UNIT").

## Verification

1. `just test-rust` -- the three new unit tests must pass. Run
   under `cargo test` filtering the new names to confirm they
   exist and run, e.g.:
   ```
   cargo test -p braid-cli cmd_lock_failure_does_not_write_done_or_stop_online
   cargo test -p braid-cli cmd_lock_success_writes_done_then_calls_mark_offline_in_order
   cargo test -p braid-cli mark_done_failure_does_not_call_mark_offline
   ```
2. `just test-rust` (full Rust suite) -- nothing else regresses.
   The seven existing `RecordingOnlineStateOps` tests at
   `cli/src/online_state.rs:443-558` must still pass because
   `coord_file_path = None` is the default and `systemctl_stop`'s
   `if let Some(...)` branch is skipped.
3. `just test-vm braid-lock-then-unlock-no-race` and
   `just test-vm braid-lock-coordinator-race` and
   `just test-vm execstop-cleans-stale-online` -- the production
   plain-lock path is structurally unchanged; the three pre-
   existing VM tests must continue to pass against the refactored
   `run_plain_lock`.
4. Regression-gate audit: each test pins a distinct ordering
   invariant, so each needs its own targeted mutation. Apply the
   mutation, run the named test, confirm it fails, then revert
   before moving on. Do not commit any of the mutated forms.

   - **Test (a)** -- `cmd_lock_failure_does_not_write_done_or_stop_online`.
     Mutation: in `cmd_lock_orchestrate_impl`, move `mark_done_fn()?`
     ahead of `cmd_lock_fn(...)?`. Expected failure mode: the
     panic-on-call `mark_done_fn` closure fires before
     `cmd_lock_fn` is consulted, and the test panics rather than
     returning a clean `Err(CmdLock(_))`. This proves test (a)
     guards against "advance to `mark_done` before checking
     `cmd_lock`'s `Err`."
   - **Test (b)** -- `cmd_lock_success_writes_done_then_calls_mark_offline_in_order`.
     Mutation: swap the order so `let _ = mark_offline(config, online_ops);`
     runs ahead of `mark_done_fn()?`. Expected failure mode: when
     `RecordingOnlineStateOps::systemctl_stop` fires inside
     `mark_offline`, the coordinator file is still the empty post-
     `acquire()` state, so `coord_snapshots()[0]` is `Vec::new()`
     rather than `b"done\n"`. This proves test (b) guards against
     "`mark_offline` before `mark_done`."
   - **Test (c)** -- `mark_done_failure_does_not_call_mark_offline`.
     Mutation: change `mark_done_fn().map_err(LockOrchestrateError::MarkDone)?;`
     to `let _ = mark_done_fn();` (swallow the error and continue).
     Expected failure mode: `mark_offline` now runs even though
     `mark_done` failed; `calls()` records a
     `"stop braid-online.service no_block=false"` entry, which
     test (c)'s "no stop call" assertion catches. This proves
     test (c) guards against "continue to `mark_offline` after a
     `mark_done` `Err`."

## Out of scope

- `run_systemd_stop_lock` extraction. That arm has different
  invariants (it polls `done\n` rather than writes it; it has no
  `mark_offline` step). Touching it would force conditional
  branches inside the orchestrator and dilute the very invariant
  the new tests pin. If that arm later grows test surface, give it
  its own seam.
- Adding `fsync` to `StopCoordinatorGuard::mark_done()`. Today's
  semantics (write-back to the held file handle, no fsync) are
  preserved.
- Differentiated exit codes per `LockOrchestrateError` variant.
  Both variants exit `1` via `print_cli_error` + `process::exit(1)`,
  matching today's behavior.
- Promotion of this plan to `plans/impl/`. That happens after the
  changes pass review.

## Meta-observation (not part of the fix)

Commit `ff6f766` promoted `plans/wip/...` to `plans/impl/...` in the
same change that landed the implementation, but skipped the §3.3
test prescription. If the `/impl-plan` workflow does not already
check that planned test names appear in the staged diff before
promotion, that is a separate process-tightening worth a follow-up.
Mentioned here for visibility, not part of this fix.
