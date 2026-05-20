# Plan: centralize the locked-command policy

## Context

The locked-command list (which `Commands::*` variants must take
`/run/braid-pool.lock` before any state read) is hand-maintained today as
inline `let _pool_guard = (!args.{,common.}dry_run).then(|| acquire_pool_or_exit(&pool_lock));`
lines scattered across 10+ match arms in `cli/src/main.rs`. The only
guardrail against accidentally dropping one of those lines is
`tests/module/pool-lock-precedes-state-read.py`, a slow NixOS VM test;
every Rust unit and integration test passes without it. A `RecordingPoolLock`
test seam exists in `cli/src/pool_lock.rs:129-203` but has zero callers,
so the seam adds maintenance burden without buying coverage.

The verify-issue investigation proposed wiring `RecordingPoolLock` into
extracted dispatch arms so per-command unit tests can assert the lock is
acquired. That preserves the underlying defect -- the locked-command list
becomes hand-maintained in two places instead of one (dispatch arms +
test list). A new `Commands` variant could still ship missing both.

This plan replaces the per-arm pattern with an exhaustive
`lock_policy(&Commands) -> LockPolicy` function. The compiler refuses to
build a new `Commands` variant until `lock_policy` explicitly classifies
it, deleting the regression class. The slow VM test stays as defense in
depth (the policy function proves the list is exhaustive; the VM test
proves dispatch actually consumes it against a real `flock`). The unused
`RecordingPoolLock` seam is removed.

## Files to modify

- `cli/src/main.rs` -- introduce `LockPolicy`, `lock_policy`, and a
  small `acquire_per_policy` helper; strip the per-arm `_pool_guard`
  lines; have dispatch acquire once at the top before entering the
  per-command match. The acquire helpers' return types tighten from
  `Box<dyn PoolLockGuard>` to `RealPoolLockGuard` (see "Trait fate"
  below).
- `cli/src/pool_lock.rs` -- delete `RecordedPoolLockMode` (lines 129-134),
  `RecordingPoolLock` plus its `AcquirePoolLock` impl (lines 136-203),
  the `AcquirePoolLock` trait itself (lines 31-44), and the
  `PoolLockGuard` marker trait + blanket impl (lines 26-29). Move the
  three acquire methods (`acquire`, `acquire_with_timeout`,
  `acquire_with_systemd_stop_deadline`) onto `RealPoolLock` directly,
  returning `RealPoolLockGuard`. See "Trait fate" below.
- `docs/decisions/026-pool-lock-rust-owned.md` -- replace the prose
  enumeration of locked commands ("for every non-dry-run command covered
  by Principle 12") with a pointer to `lock_policy` as the single source
  of truth, and note that the wildcard-free exhaustive match enforces
  the invariant at compile time.
- `docs/principles.md` -- rewrite Principle 12 (line 65) so it describes
  the *categories* of lock policy and the *invariants* (acquire before
  state read; fail-fast vs timeout vs silent-exit semantics; bare
  `discover` exception; `unlock`'s post-lock mount re-check) but drops
  the two inline enumerations of `Commands` variants. Both lists point
  at `lock_policy` in `cli/src/main.rs` as the authoritative
  variant-to-discipline mapping. See "Principle 12 rewrite" below.

## Design

### `LockPolicy` enum

Lives in `cli/src/main.rs` (private; mirrors the location of `Commands`).

```rust
enum LockPolicy {
    None,
    NonBlocking,
    Timeout(Duration),
    MonitorSilent,         // acquire non-blocking; exit 0 silently on AlreadyHeld
    LockPlain,             // delegates to run_plain_lock
    LockSystemdStop(Duration), // delegates to run_systemd_stop_lock
}
```

Six variants -- one per acquisition discipline observed in main.rs today:
- `None`: read-only or dry-run paths.
- `NonBlocking`: fail-fast mutators (`acquire_pool_or_exit`).
- `Timeout(Duration)`: `Ack` (`acquire_pool_with_timeout_or_exit`).
- `MonitorSilent`: `Monitor`'s `acquire()` + exit-0-on-AlreadyHeld branch.
- `LockPlain` / `LockSystemdStop`: delegate to existing helpers that
  manage the stop coordinator alongside the pool lock.

### `lock_policy` function

Exhaustive match -- no wildcard.

```rust
fn lock_policy(command: &Commands) -> LockPolicy {
    use Commands::*;
    use LockPolicy::*;
    match command {
        // Fail-fast mutators
        Add(a)            => if a.common.dry_run { None } else { NonBlocking },
        Remove(a)         => if a.common.dry_run { None } else { NonBlocking },
        RemoveMissing(a)  => if a.common.dry_run { None } else { NonBlocking },
        Replace(a)        => if a.common.dry_run { None } else { NonBlocking },
        Unlock(a)         => if a.dry_run        { None } else { NonBlocking },
        EnrollKeyFile(a)  => if a.dry_run        { None } else { NonBlocking },
        Recover(a)        => if a.dry_run        { None } else { NonBlocking },
        Discover(a)       => if a.write          { NonBlocking } else { None },
        // Special cases
        Ack               => Timeout(Duration::from_secs(10)),
        Monitor           => MonitorSilent,
        Lock(a) => {
            if a.dry_run { None }
            else if a.systemd_stop {
                LockSystemdStop(Duration::from_secs(
                    a.deadline_secs.expect("clap requires deadline"),
                ))
            } else {
                LockPlain
            }
        }
        // Read-only / no-lock commands
        Status(_) | Doctor(_) | Idle | ScrubCancel(_)
            | ScrubNeedsResume(_) | ScrubResumeOrStart(_)
            | Tui(_) | Ups(_) | Help(_) => None,
    }
}
```

### Dispatch shape

The top of `main()` becomes:

```rust
let policy = lock_policy(&cli.command);
let _pool_guard: Option<RealPoolLockGuard> = match &policy {
    LockPolicy::None | LockPolicy::LockPlain | LockPolicy::LockSystemdStop(_) => None,
    LockPolicy::NonBlocking => Some(acquire_pool_or_exit(&pool_lock)),
    LockPolicy::Timeout(d)  => Some(acquire_pool_with_timeout_or_exit(&pool_lock, *d)),
    LockPolicy::MonitorSilent => match pool_lock.acquire() {
        Ok(guard) => Some(guard),
        Err(PoolLockError::AlreadyHeld) => std::process::exit(0),
        Err(e) => { handle_pool_lock_error(e); std::process::exit(2); }
    },
};
match cli.command {
    Commands::Add(args) => { /* no more _pool_guard line; cmd_add(...) */ }
    // ...
    Commands::Lock(args) => {
        // Lock arm still routes to run_plain_lock / run_systemd_stop_lock
        // because those helpers manage the stop coordinator + pool lock
        // jointly. The LockPlain / LockSystemdStop policy variants exist
        // so the exhaustive match still proves Lock has been classified;
        // top-level acquisition is intentionally None for them.
    }
}
```

Guard type is the concrete `RealPoolLockGuard` (no `Box<dyn ...>`)
because the only acquirer is `RealPoolLock`. The `PoolLockGuard` marker
trait existed only to let `RecordingPoolLock` return `Box<()>`; with the
recording impl gone, the indirection has no callers.

This preserves the special stop-coordinator-then-pool-lock ordering used
by `Lock` (see `run_plain_lock` and `run_systemd_stop_lock` at
`cli/src/main.rs:981-1050+`), while still routing `Lock` through the
exhaustive policy table. The classification is captured even though
acquisition is delegated.

### Trait fate (`AcquirePoolLock` and `PoolLockGuard`)

Both traits at `cli/src/pool_lock.rs:26-44` were introduced as seams
for `RecordingPoolLock`:

- `PoolLockGuard` (lines 26-29) is a marker trait with a blanket impl
  so `RecordingPoolLock` could return `Box<()>` while `RealPoolLock`
  returned `Box<RealPoolLockGuard>`.
- `AcquirePoolLock` (lines 31-44) is the acquire-method seam itself.

With `RecordingPoolLock` deleted, both traits have one impl each and
zero test consumers. Delete both:

- Inline the three acquire methods (`acquire`, `acquire_with_timeout`,
  `acquire_with_systemd_stop_deadline`) directly on `RealPoolLock`,
  returning `RealPoolLockGuard` (not `Box<dyn PoolLockGuard>`).
- Drop the `AcquirePoolLock` and `PoolLockGuard` imports from
  `cli/src/main.rs`.
- The `acquire_pool_or_exit` / `acquire_pool_with_timeout_or_exit`
  helpers in `main.rs:929-950` return `RealPoolLockGuard`.

Single-impl traits without test seams are dead abstraction; removing
them aligns with the project's "no abstractions beyond what the task
requires" rule and makes the call sites concrete throughout.

### Principle 12 rewrite

The current Principle 12 paragraph at `docs/principles.md:65-67`
enumerates the locked-command set twice in prose (once as "pool
mutators, alert-state mutators, ..." and again as the fail-fast
list). That duplication is the second drift surface after the
per-arm `_pool_guard` lines. Rewrite the principle to describe
*categories and invariants* only:

- **Invariant.** Rust dispatch acquires `/run/braid-pool.lock` before
  loading config, loading `pool.json`, probing pool state, or
  prompting. The authoritative `Commands`-to-acquire-discipline
  mapping lives in `lock_policy` in `cli/src/main.rs`; that
  function's wildcard-free exhaustive match is what makes this
  principle compiler-enforced.
- **Categories.** Describe the four disciplines without naming
  specific commands: fail-fast non-blocking (with the "another braid
  operation is already in progress" rationale); bounded timeout (for
  cycles where short contention is normal); silent-exit-on-contention
  (for periodic timer-driven cycles whose miss is harmless); and
  no-lock (for read-only paths and dry-run modes).
- **Architectural choice.** Keep the existing sentence about mutual
  exclusion at the critical section rather than via systemd unit
  topology.
- **Post-lock re-check.** Keep the existing sentence about `unlock`'s
  mount re-check under the held lock.

The bare `discover` exception, the 10-second `ack` wait, and the
`monitor` silent-exit explanation all become category illustrations
rather than command-by-command rules. Concrete command names appear
only as cross-references ("see `lock_policy`"), not as authoritative
lists.

## What stays the same

- `tests/module/pool-lock-precedes-state-read.py` -- defense in depth.
  The policy function proves the list is exhaustive; this VM test
  proves dispatch actually consumes it against a real `flock(2)`.
- All other `tests/module/pool-lock-*.py` and
  `tests/module/braid-pool-lock-*.py` tests -- they exercise lock
  contention semantics, not the dispatch list.
- The helpers `acquire_pool_or_exit`,
  `acquire_pool_with_timeout_or_exit`, `handle_pool_lock_error`,
  `run_plain_lock`, `run_systemd_stop_lock` -- bodies unchanged.
  Only the call sites move and the return types of the two acquire
  helpers tighten to `RealPoolLockGuard`.
- The `needs_root` match at `cli/src/main.rs:374-378` -- out of scope
  for this pivot. (Promoting it to a similar exhaustive pattern is a
  separate refactor.)

## Verification

1. **Build and unit tests.** `just test-rust` -- the existing
   `cli/src/main.rs` clap-parsing tests (around lines 1320-1370) and
   `cli/src/pool_lock.rs` tests (lines 299-422) must continue passing.
2. **Complete policy table test** in `cli/src/main.rs`'s
   `#[cfg(test)] mod tests`. Compile-time exhaustiveness only proves
   every variant is *classified*; it does not prove it is *classified
   correctly*. Cover every `Commands` variant *and every behavior
   branch* (`dry_run`, `--write`, `--systemd-stop`) so a swap like
   `Unlock --dry-run` -> `NonBlocking` or `Doctor` -> `NonBlocking`
   is caught by the unit suite, not the VM lane. The full required
   table:

   | Command            | Input                                     | Expected `LockPolicy`           |
   | ------------------ | ----------------------------------------- | ------------------------------- |
   | `Add`              | `dry_run=false`                           | `NonBlocking`                   |
   | `Add`              | `dry_run=true`                            | `None`                          |
   | `Remove`           | `dry_run=false`                           | `NonBlocking`                   |
   | `Remove`           | `dry_run=true`                            | `None`                          |
   | `RemoveMissing`    | `dry_run=false`                           | `NonBlocking`                   |
   | `RemoveMissing`    | `dry_run=true`                            | `None`                          |
   | `Replace`          | `dry_run=false`                           | `NonBlocking`                   |
   | `Replace`          | `dry_run=true`                            | `None`                          |
   | `Unlock`           | `dry_run=false`                           | `NonBlocking`                   |
   | `Unlock`           | `dry_run=true`                            | `None`                          |
   | `EnrollKeyFile`    | `dry_run=false`                           | `NonBlocking`                   |
   | `EnrollKeyFile`    | `dry_run=true`                            | `None`                          |
   | `Recover`          | `dry_run=false`                           | `NonBlocking`                   |
   | `Recover`          | `dry_run=true`                            | `None`                          |
   | `Discover`         | `write=true`                              | `NonBlocking`                   |
   | `Discover`         | `write=false`                             | `None`                          |
   | `Lock`             | `dry_run=true`                            | `None`                          |
   | `Lock`             | `dry_run=false, systemd_stop=false`       | `LockPlain`                     |
   | `Lock`             | `dry_run=false, systemd_stop=true, d=270` | `LockSystemdStop(270s)`         |
   | `Ack`              | --                                        | `Timeout(10s)`                  |
   | `Monitor`          | --                                        | `MonitorSilent`                 |
   | `Status`           | --                                        | `None`                          |
   | `Doctor`           | --                                        | `None`                          |
   | `Idle`             | --                                        | `None`                          |
   | `ScrubCancel`      | --                                        | `None`                          |
   | `ScrubNeedsResume` | --                                        | `None`                          |
   | `ScrubResumeOrStart` | --                                      | `None`                          |
   | `Tui`              | --                                        | `None`                          |
   | `Ups`              | --                                        | `None`                          |
   | `Help`             | --                                        | `None`                          |

   Driven from `Cli::try_parse_from(...)` (matches the existing
   `cli/src/main.rs:1320-1370` clap-parsing test style) so the policy
   is exercised against the same input shape the binary sees.
   These tests are pure pattern matches and run in milliseconds.
3. **Compile-time exhaustiveness check.** Adding a new dummy
   `Commands::DummyVariant` (locally, then reverted) must produce a
   `non-exhaustive patterns` error from `lock_policy`. This is the
   load-bearing invariant; calling it out during code review is enough,
   no test needed.
4. **VM regression test.** `just test-vm pool-lock-precedes-state-read`
   must still pass, proving the new dispatch shape preserves the
   "acquire before any state read" ordering for every covered command.
5. **Wider VM smoke.** `just test-vm` (full suite, no `-v`) to catch
   any regression in `Lock`'s plain vs systemd-stop ordering, the
   `Monitor` silent-exit semantics, or `Ack`'s 10s wait. These three
   are the most likely to break under the dispatch reshuffle.

## Out of scope

- Promoting `needs_root` to an exhaustive policy.
- Extracting `main()` into a testable `run(cli, deps)` function.
- Any change to `tests/module/pool-lock-*.py` or the other locking VM
  tests.
- Reworking the stop-coordinator protocol or `Lock`'s systemd-stop
  deadline behavior.
