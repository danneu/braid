# Plan: pivot the "discover --write lock-precedence test gap" finding

## Context

A plan-review finding flagged that `tests/module/pool-lock-precedes-state-read.py`
does not pin that `pool.json` reads happen post-lock on the
`braid discover --write` path. The original finding proposed a diagnostic-check
test (pre-seed a Corrupt pool.json, assert no classifier-rejected diagnostic).

The `verify-issue` investigation showed two things, and a follow-up reviewer
pass corrected one of them:

- The diagnostic-check test as originally proposed cannot distinguish the
  regression: the "pool.json at X is corrupt or unreadable" diagnostic only
  fires inside the `!args.write` branch, so it cannot fire on `--write`
  regardless of whether `classify_pool_json` runs pre-lock or post-lock.
  Existing coverage at `pool-lock-precedes-state-read.py:70-84` proves the
  pending-op gate and the device probe run post-lock, but it does *not*
  trap a pre-lock `pool.json` read that discards its result.
- There IS a way to pin the invariant behaviorally: replace `pool.json`
  with a blocking FIFO under the held lock. If any code reads `pool.json`
  before the lock acquire, `std::fs::read_to_string` (called by
  `load_membership_from` at `cli/src/membership.rs:431`, which
  `classify_pool_json` uses) blocks on the FIFO with no writer and the
  command times out instead of printing the contention message. If the
  invariant holds, the nonblocking lock acquire fails fast under contention,
  the process exits 1 with the contention message, and the FIFO is never
  opened.

Principle 12 (`docs/principles.md:67`) and ADR 018
(`docs/decisions/018-systemd-lifecycle.md`) require the pool lock to
precede any `pool.json` load on locked dispatch arms. This plan does two
small things: (1) add the FIFO behavioral trap so the invariant is
actually pinned for `discover --write`, and (2) clean up the dead
unconditional `classify_pool_json` call on the `--write` path so the
dispatch arm matches its intent.

Lock-policy note: `acquire_pool_or_exit` (`cli/src/main.rs:929`) calls
`RealPoolLock::acquire`, which uses `LockExclusiveNonblock` at
`cli/src/pool_lock.rs:74`. It is a nonblocking acquire that either returns
a guard or exits the process via `handle_pool_lock_error` + `std::process::exit(1)`
on `AlreadyHeld`. It does not wait.

Intended outcome: a behavioral test that catches any future regression
that reads `pool.json` before the pool lock on `discover --write`, plus
a small simplification of the `Discover` dispatch arm.

## Change 1: FIFO behavioral trap (test)

Add a new subtest to `tests/module/pool-lock-precedes-state-read.py`,
placed immediately after the existing "discover --write acquires before
pending-op and probe reads" subtest (current lines 70-84):

```python
with subtest("discover --write does not read pool.json before acquiring lock"):
    # Intent: pool.json must not be opened before the pool lock is held.
    # Why: principle 12 (`docs/principles.md`) and ADR 018 require lock
    # acquire to precede any pool.json load on locked dispatch arms.
    # A regression that reads pool.json pre-lock would not be caught by
    # the existing pending-op / probe assertions because the classify
    # result is discarded on --write and produces no diagnostic.
    # Scenario: external holder holds /run/braid-pool.lock; pool.json
    # is a blocking FIFO with no writer. Under the invariant, the
    # nonblocking flock acquire fails fast and the command exits with
    # the contention message before any pool.json read. A regression
    # that moves classify_pool_json (or any other pool.json read)
    # above the lock acquire would block on the FIFO open and time out.
    machine.succeed("mkdir -p /var/lib/braid")
    machine.succeed("rm -f /var/lib/braid/pool.json")
    machine.succeed("mkfifo /var/lib/braid/pool.json")
    try:
        rc, out = with_holder("braid discover --write --expect-count 1")
        assert rc != 0, "discover --write should fail under contention; out=" + out
        assert rc != 124, (
            "discover --write read pool.json before acquiring lock (timed out); "
            "out=" + out
        )
        assert "another braid operation is already in progress" in out, (
            "expected contention message; out=" + out
        )
    finally:
        machine.succeed("rm -f /var/lib/braid/pool.json")
```

Design notes:

- The FIFO MUST be removed in `finally` so it does not leak into later
  subtests (`ack`, `monitor`).
- The `with_holder` helper (`pool-lock-precedes-state-read.py:23-36`)
  releases the holder via `wait_until_succeeds` in its own `finally`,
  so the FIFO trap runs entirely while the lock is held.
- `rc != 124` is the load-bearing assertion -- it pins that the command
  did not hang. The `rc != 0` assertion plus the contention substring
  check guard against false success (e.g. a regression where the FIFO
  read returned EOF immediately and the command proceeded).
- Re-validate the test before relying on it (per `AGENTS.md` TDD note):
  temporarily move `classify_pool_json(&pool_json)` above the `_pool_guard`
  binding in `cli/src/main.rs` and confirm the new subtest fails with
  `rc == 124`. Revert the regression after confirming.

## Change 2: cleanup (code)

Single edit to `cli/src/main.rs`, inside `Commands::Discover(args) =>`
(lines 794-823). Move the shape computation inside the existing
`if !args.write { }` block. With Change 1 in place this is independently
worth doing -- it removes a dead `pool.json` read on the `--write` path
and makes the read-only path's intent literal (the classifier exists
only to gate the bare path's early exit).

Today:

```rust
let _pool_guard = args.write.then(|| acquire_pool_or_exit(&pool_lock));
// Note: ...comment block about gates...
let pool_json = paths.pool_json();
let shape = braid_cli::discover::classify_pool_json(&pool_json);
if !args.write {
    match shape {
        braid_cli::discover::PoolJsonShape::Missing => {}
        braid_cli::discover::PoolJsonShape::ValidUuidKeyed => { ... exit ... }
        braid_cli::discover::PoolJsonShape::Corrupt => { ... exit ... }
    }
}
```

After:

```rust
let _pool_guard = args.write.then(|| acquire_pool_or_exit(&pool_lock));
// Note: ...comment block about gates (unchanged)...
let pool_json = paths.pool_json();
if !args.write {
    match braid_cli::discover::classify_pool_json(&pool_json) {
        braid_cli::discover::PoolJsonShape::Missing => {}
        braid_cli::discover::PoolJsonShape::ValidUuidKeyed => { ... exit ... }
        braid_cli::discover::PoolJsonShape::Corrupt => { ... exit ... }
    }
}
```

Notes:

- `pool_json` (line 803) stays outside the block. It is consumed by the
  `--write` success message at line 847 and the bare hint at line 855.
- The intermediate `let shape = ...` binding is inlined into the `match`
  head since it had no other consumer. One less local, call site
  visually adjacent to its consumer.
- The comment at lines 796-802 stays put. It already explains both paths
  and remains accurate after the move.
- `bool::then` evaluates its closure only when the receiver is `true`
  (per `core::primitive::bool::then`), so the lock acquire is skipped
  entirely on the bare path.

## Files modified

- `tests/module/pool-lock-precedes-state-read.py` -- add the FIFO subtest
  after the existing "discover --write acquires before pending-op and
  probe reads" subtest at lines 70-84.
- `cli/src/main.rs` -- lines 803-823 only.

## Not modified (and why)

- `cli/src/discover.rs` -- `classify_pool_json` itself is unchanged. Its
  callers in `write_discovered_membership` (`discover.rs:575`) and its
  unit tests (`discover.rs:2022+`) are unaffected.
- `tests/cli/braid-discover.py` -- already exercises the bare-discover
  Corrupt and ValidUuidKeyed refusal paths (e.g. `assert_corrupt_preview_refuses`
  at lines 47-58, used at 108, 117); behavior of those paths is identical
  after the code move.
- `tests/module/pool-lock-discover-contention.py` -- already pins the
  lock-released ValidUuidKeyed refusal at lines 65-76; that path runs
  through `write_discovered_membership`'s internal classify call, which
  is unaffected by Change 2.

## Verification

- `just test-rust` -- compiles and runs unit tests; covers
  `classify_pool_json_*` directly.
- TDD pre-check on the new subtest: temporarily hoist the
  `classify_pool_json` call above the `_pool_guard` binding in
  `cli/src/main.rs`, run `just test-vm pool-lock-precedes-state-read`,
  confirm the new subtest fails with `rc == 124`, then revert the
  regression.
- `just test-vm pool-lock-precedes-state-read pool-lock-discover-contention braid-discover`
  -- runs the three VM tests that exercise the affected dispatch arm.
  All three must pass after both changes are in place.
- Manual sanity: read the diff -- the body move must preserve the
  match arms and their error messages verbatim (no reordering, no
  message edits).

## Implementation notes

- `cli/src/main.rs` now acquires dispatch locks through the centralized
  `acquire_per_policy` call before the command match, so the temporary TDD
  regression hoisted `classify_pool_json` above that centralized acquire
  rather than above a `Discover`-local guard.
