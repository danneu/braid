# Plan: cover the `poll_acquire` success-after-mid-poll path

## Context

`cli/src/pool_lock.rs` exposes two timed acquire wrappers --
`acquire_with_timeout` (the bounded wait used exclusively by `cmd_ack`,
`main.rs:772-774`) and `acquire_with_systemd_stop_deadline` (used by
`braid lock --systemd-stop`). Both wrap the shared
`RealPoolLock::poll_acquire` retry loop at `cli/src/pool_lock.rs:81-97`,
which is the only place in the file that exercises
`start.elapsed() < timeout`, `thread::sleep(POOL_POLL_INTERVAL)`, and
re-trying after a contended `LockExclusiveNonblock`.

Today the unit tests cover only the uncontested and expiry branches:

| Branch                              | Test                                                                    |
| ----------------------------------- | ----------------------------------------------------------------------- |
| `acquire` uncontested               | `acquire_returns_already_held_on_second_attempt` (line 322)             |
| `acquire_with_timeout` expiry       | `acquire_with_timeout_returns_already_held_on_expiry` (line 334)        |
| `acquire_with_systemd_stop` expiry  | `acquire_with_systemd_stop_deadline_returns_deadline_expired_on_expiry` (line 346) |
| **`poll_acquire` succeeds mid-wait**| **MISSING for both wrappers**                                           |

The two missing `*_polls_then_succeeds` tests were explicitly listed in
the implementation plan
(`plans/impl/2026-05-19-rust-owned-pool-operation-lock.md:1241` and
`:1245`) but never landed.
`git log -S "acquire_with_timeout_polls" -- cli/src/pool_lock.rs`
returns nothing (the unqualified `-S` form would surface the plan-
introducing commit and obscure the point).

The existing VM tests cannot substitute:

- `tests/module/alert-state-lock.py:118` holds the lock with
  `sleep 60`; the ack subtest at lines 230-240 asserts the wait is
  `9 <= elapsed <= 14` -- i.e. expiry.
- `tests/module/pool-lock-precedes-state-read.py:86-99` holds for 12s
  and asserts the same 9-14s window -- also expiry.

A regression in `poll_acquire`'s retry loop -- off-by-one on
`start.elapsed() < timeout`, exit-on-first-`AlreadyHeld`, or a future
change that turns the polled `LockExclusiveNonblock` into a one-shot
try -- would pass every existing Rust unit and VM test. `braid ack`
would then refuse to wait out a short concurrent operation even though
the shipped wrapper used to.

## Pivot from the original finding

The finding cited only the `acquire_with_timeout` half of the gap (the
ack-facing one). Both wrappers share the same `poll_acquire`
implementation, both were plan-mandated, and the file already pairs
every `*_on_expiry` test with no positive partner. Filling only one
half leaves the symmetric sibling uncovered for ~10 lines of extra
test code. The ideal pivot adds both tests.

## Approach

Add two `#[test]` functions to the `tests` mod in `cli/src/pool_lock.rs`,
each placed immediately after its `*_on_expiry` partner so pairs read
together:

1. `acquire_with_timeout_polls_then_succeeds_after_holder_release` --
   inserted after `acquire_with_timeout_returns_already_held_on_expiry`
   (line 343).
2. `acquire_with_systemd_stop_deadline_polls_then_succeeds_after_holder_release` --
   inserted after
   `acquire_with_systemd_stop_deadline_returns_deadline_expired_on_expiry`
   (line 355).

Both follow the established `tempfile::tempdir()` pattern used by every
existing test in the module (no mock seam -- exercise real kernel
`flock` semantics).

### Concurrency fixture (Send-safe and deterministic)

`pool_lock.rs:28` defines `pub trait PoolLockGuard {}` with no `Send`
bound, so `Box<dyn PoolLockGuard>` returned by
`AcquirePoolLock::acquire` cannot cross `thread::spawn`. To get a
**Send** holder guard, the test calls the private
`RealPoolLock::try_acquire()` (`pool_lock.rs:72-79`), which returns
the concrete `pub struct RealPoolLockGuard { _file: File }`
(`pool_lock.rs:125-127`). `File: Send`, so `RealPoolLockGuard: Send`
automatically. The private fn is visible to the `tests` child
module via `use super::*;`. The test uses it only as a fixture
primitive; assertions remain against the public timed wrappers.

Fixture roles (this is the inversion of the prior revision):

- **Main thread** acquires the holder via `try_acquire`, spawns a
  releaser thread that **owns** the concrete `RealPoolLockGuard`,
  then immediately calls the timed wrapper under test. Main never
  sleeps before calling the wrapper, so its first `try_acquire`
  (inside `acquire_with_timeout`) runs microseconds after the spawn
  -- well within the releaser's 100 ms hold window. Contention is
  guaranteed on the main thread's first attempt regardless of
  releaser scheduling jitter.
- **Releaser thread** sleeps 100 ms, then drops the guard. Total
  hold duration is >= 100 ms measured from when the releaser thread
  actually starts; even with adverse scheduling, the lock is held
  long enough to be observed by main.

Per-Linux `flock(2)` semantics: two file descriptors opened on the
same path conflict (already exercised by
`acquire_returns_already_held_on_second_attempt` at
`pool_lock.rs:322-331`).

### Test shape (both tests identical except for the wrapper called and the preamble)

```rust
// Intent: acquire_with_timeout returns Ok when the holder releases
//   mid-poll, exercising poll_acquire's sleep-then-retry branch -- not
//   only the uncontested fast path or the expiry path.
// Why it exists: protects the only positive-shape gate for
//   `braid ack`'s bounded wait. A regression in poll_acquire -- an
//   off-by-one on `start.elapsed() < timeout`, exit-on-first
//   AlreadyHeld, or a change that turns the polled
//   LockExclusiveNonblock into a one-shot try -- would silently make
//   ack refuse to wait out a short concurrent operation while every
//   existing Rust unit and VM test still passed (the VM tests hold
//   the lock past ack's 10s timeout, so they only cover expiry).
// Scenario: `braid ack` runs while a concurrent monitor cycle briefly
//   holds the pool lock; ack should observe the release within its
//   bounded wait window and proceed.
#[test]
fn acquire_with_timeout_polls_then_succeeds_after_holder_release() {
    let dir = tempfile::tempdir().unwrap();
    let lock = RealPoolLock::new(dir.path().join("pool.lock"));

    let holder = lock.try_acquire().expect("initial holder acquire");
    let releaser = thread::spawn(move || {
        thread::sleep(Duration::from_millis(100));
        drop(holder);
    });

    let start = Instant::now();
    let result = lock.acquire_with_timeout(Duration::from_secs(2));
    let elapsed = start.elapsed();
    releaser.join().expect("releaser panicked");

    assert!(
        result.is_ok(),
        "expected Ok after holder release; got {:?}",
        result.err()
    );
    assert!(
        elapsed >= POOL_POLL_INTERVAL,
        "main thread did not exercise the retry path; elapsed={:?}",
        elapsed
    );
}
```

The sibling test substitutes
`acquire_with_systemd_stop_deadline(Duration::from_secs(2))` and a
preamble that swaps "ack" / "monitor cycle" framing for "braid lock
--systemd-stop" / "concurrent mutator at shutdown."

### Timing rationale

- `POOL_POLL_INTERVAL = 250 ms` (`pool_lock.rs:22`). Main's first
  `try_acquire` happens within microseconds of the `thread::spawn`
  call returning -- always within the releaser's 100 ms hold. Main
  observes `AlreadyHeld`, sleeps `POOL_POLL_INTERVAL`, retries.
  Expected `elapsed` is ~250-350 ms.
- A 2 second timeout leaves >1.5 s of headroom for slow CI without
  ever risking expiry.
- `assert!(elapsed >= POOL_POLL_INTERVAL)` confirms the retry path
  was taken. Because main does not sleep before calling the wrapper,
  the only way `elapsed` could fall below `POOL_POLL_INTERVAL` is if
  the holder had already been dropped before main started -- which
  the structure prevents.

## Critical files

- **Edit:** `cli/src/pool_lock.rs` -- append two `#[test]` fns to the
  `tests` mod (no production code changes).

## Out of scope

- Production code (`poll_acquire`, the wrappers, `POOL_POLL_INTERVAL`,
  the `PoolLockGuard` trait's `Send` bound) is unchanged.
- No VM test edits. The Rust unit-test bar is sufficient for the
  shared `poll_acquire` branch; the existing VM tests already cover
  user-visible expiry semantics.
- No retrofit of preambles onto the pre-existing pool_lock tests.
  New tests adopt the convention from `docs/testing.md:11-22`; the
  pre-existing debt is out of scope for this pivot.
- No update to
  `plans/impl/2026-05-19-rust-owned-pool-operation-lock.md`. The plan
  is historical; the new tests close the gap it described.

## Verification

1. `just test-rust` -- both new tests must pass. Expected runtime
   ~250-350 ms each; total Rust suite delta well under a second.
2. Negative sanity check that the new tests actually exercise
   `poll_acquire`'s sleep branch: temporarily change
   `start.elapsed() < timeout` to `start.elapsed() < Duration::ZERO`
   in `pool_lock.rs:90` (forces immediate expiry on first
   `AlreadyHeld`). Both new tests must fail with the `expected Ok
   after holder release` assertion. Revert.
3. Deterministic negative sanity check that the elapsed canary bites:
   temporarily change the test body to drop the holder *before* the
   timed wrapper call (i.e. move `drop(holder)` above the
   `lock.acquire_with_timeout(...)` line and remove the releaser
   thread). The wrapper's first `try_acquire` then succeeds
   immediately, `elapsed` is sub-millisecond, and the
   `elapsed >= POOL_POLL_INTERVAL` assertion fails. Revert.
4. `just test-rust` after both reverts; full suite green.
