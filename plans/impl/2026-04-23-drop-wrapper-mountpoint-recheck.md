# plan-the-fix-distributed-moon

Remove the wrapper's duplicate `mountpoint -q` re-check on `braid unlock`.
The CLI's own check inside `plan_open_pool` already runs under the wrapper's
held flock and prints the same message, so the wrapper branch is dead
weight.

## Context

`feature-findings/unlock.md` flagged a simplicity finding: the wrapper
(`modules/braid/braid-wrapper.sh:52-61`) and the CLI
(`cli/src/mount.rs:163-170`) both perform `mountpoint -q` on the pool mount
point for `braid unlock` and both print the literal string
`pool already mounted at <path>` before exiting 0. Verification confirmed
the duplication.

Both checks run under the wrapper's held `flock(2)` on
`/run/braid-pool.lock`: the wrapper opens FD 9 at `braid-wrapper.sh:39-43`,
then invokes the CLI (`@braidBin@ "$@"` at line 78, *not* `exec`) and keeps
running post-fixup at lines 81-109 while that FD stays open. The FD is
inherited by the CLI subprocess, so the CLI's own re-check is within the
same critical section. Removing the wrapper branch preserves the Principle
12 contract intact.

Two subtleties the plan must handle explicitly:

1. **Stream change.** Wrapper emits to stdout (`echo`); CLI emits to stderr
   (`eprintln!`). Decision: keep stderr -- consistent with every other
   braid diagnostic. This is a user-visible behavior change that needs a
   test gate (see below).

2. **Doc drift.** Principle 12 and Decision 018 currently describe the
   re-check in wording that reads as a wrapper step. The rewording must
   stay at the contract level -- "`unlock` re-checks mount state under the
   held lock" -- without naming `plan_open_pool` or claiming the wrapper
   `exec`s the CLI (it does not). Pinning the doc to an internal helper
   name or to process semantics the wrapper doesn't actually have would
   make the architecture docs *less* accurate than the code.

## Changes

### 1. `modules/braid/braid-wrapper.sh`

Delete lines 48-61 (leading comment + the `case "$subcmd" in unlock) ...
esac` that runs `mountpoint -q` and echoes the duplicate message).

Keep untouched:
- Flock acquisition at lines 30-46 (Principle 12: mutual exclusion).
- Post-exec fixup at lines 81-109. Its `mountpoint -q` at line 84 is a
  different check -- "did the CLI actually mount?" gating chown/chmod/
  online-service activation -- not a duplicate.
- `skip_fixup` handling for `--dry-run` / `--help` / `--version`.

### 2. `docs/principles.md:58-60` (Principle 12 body)

Reword the "after acquiring the lock" sentence to describe the contract
without pinning to an implementation helper. Proposed wording:

> Pool-mutating commands (`unlock`, `add`, `recover`) acquire an exclusive
> **non-blocking** `flock` on `/run/braid-pool.lock` for their duration.
> braid does not queue pool operations: a concurrent attempt fails fast
> with `braid: another braid operation is already in progress` and the user
> must retry once the active operation completes. Mutual exclusion is
> enforced at the critical section itself, not via systemd unit topology.
> Under the held lock, `unlock` re-checks whether the pool is already
> mounted and exits cleanly if a prior winner mounted it sequentially;
> `add` and `recover` do not fast-exit because they legitimately operate
> on mounted pools.

No mention of `plan_open_pool`, no mention of `exec`, no mention of which
file holds the check.

### 3. `docs/decisions/018-systemd-lifecycle.md:135`

Mirror the Principle 12 rewording in the same contract-level phrasing.
Specifically change the existing clause "After acquiring the lock, `unlock`
re-checks `mountpoint -q` and exits cleanly..." to "Under the held lock,
`unlock` re-checks whether the pool is already mounted and exits cleanly
..." and leave the rest of the paragraph alone.

### 4. `tests/cli/braid-unlock.py` -- add dedicated already-mounted subtest

Current coverage gap: the existing "Test 2: idempotent" subtest
(`braid-unlock.py:92-98`) verifies data integrity after a second unlock
but never asserts exit code, message text, or stream routing. The VM
concurrent-unlock test (`tests/module/systemd-lifecycle.py:170-197`)
captures via `2>&1` and only hits the sequential-loser branch
nondeterministically. The Rust unit test at `cli/src/mount.rs:780-803`
exercises `plan_open_pool` in isolation with a mock runner, not the
wrapper -> `cmd_unlock` wiring.

Add a new subtest immediately after Test 2 (before Test 2b at line 100):

- **Intent:** When the pool is already mounted, `braid unlock` exits 0,
  emits `pool already mounted at /mnt/storage` to **stderr** (not stdout),
  performs no cryptsetup/mount work, and leaves the pool unchanged.
- **Why:** The wrapper's pre-CLI `mountpoint -q` short-circuit was removed
  in favor of the CLI's own check. Without this test, a regression could
  silently re-route the message back to stdout, or drop it entirely, or
  drop the `Ok(None)` short-circuit in `plan_open_pool` and cause
  redundant mount work -- all exit-0 and all invisible to existing tests.
- **Scenario:** Same as Test 2 (pool mounted from Test 1), but captures
  stdout and stderr to separate files and asserts shape explicitly.

Sketch:

```python
with subtest("Test 2c: already-mounted unlock -> exit 0, stderr message, no remount"):
    # Precondition: pool is mounted from Test 2
    machine.succeed("mountpoint -q /mnt/storage")

    # Snapshot mount id and mapper set to prove no remount/reopen work
    before_src = machine.succeed("findmnt -n -o SOURCE /mnt/storage").strip()
    before_mappers = machine.succeed(
        "ls /dev/mapper/ | grep '^braid-' | sort"
    ).strip()

    # Run with stdout/stderr split. machine.succeed asserts exit 0.
    machine.succeed(
        f"{unlock_cmd(passphrase)} >/tmp/amm-stdout 2>/tmp/amm-stderr"
    )
    out = machine.succeed("cat /tmp/amm-stdout")
    err = machine.succeed("cat /tmp/amm-stderr")

    # Message is on stderr, absent from stdout
    assert "pool already mounted" in err, \
        f"expected 'pool already mounted' on stderr; stderr={err!r} stdout={out!r}"
    assert "pool already mounted" not in out, \
        f"message leaked to stdout; stdout={out!r}"

    # No remount (same mount source) and same mapper set
    after_src = machine.succeed("findmnt -n -o SOURCE /mnt/storage").strip()
    after_mappers = machine.succeed(
        "ls /dev/mapper/ | grep '^braid-' | sort"
    ).strip()
    assert before_src == after_src, \
        f"mount source changed: before={before_src} after={after_src}"
    assert before_mappers == after_mappers, \
        f"mapper set changed: before={before_mappers} after={after_mappers}"
```

Placement note: this subtest relies on Test 1 having mounted the pool and
Test 2 having left it mounted. Must go after Test 2 and before Test 2b
(which calls `close_all()`).

## Files

Modified:
- `modules/braid/braid-wrapper.sh` (delete lines 48-61)
- `docs/principles.md` (Principle 12 body, lines 58-60)
- `docs/decisions/018-systemd-lifecycle.md` (the re-check clause around
  line 135)
- `tests/cli/braid-unlock.py` (add Test 2c between existing Test 2 and
  Test 2b)

Referenced (unchanged but relied upon):
- `cli/src/mount.rs:163-170` -- the authoritative `mountpoint -q` check
  inside `plan_open_pool`; `eprintln!` at line 168 stays on stderr
- `cli/src/unlock.rs:65-68` -- `Ok(None)` -> exit 0 no-op path

## Verification

1. `just test-rust` -- `mount_already_mounted_returns_false`
   (`cli/src/mount.rs:780-803`) still passes; no new Rust test needed
   because the new behavioral gate lives at the VM layer where
   stream-routing is observable.
2. `just test-vm braid-unlock` -- the new Test 2c passes. Confirm it would
   **fail** if reverted by re-adding the wrapper echo to stdout (manual
   pre-merge sanity check, not a committed mutation).
3. `just test-vm systemd-lifecycle` -- concurrent-unlock loser-path
   subtest still passes; text match via `2>&1` at
   `systemd-lifecycle.py:155` works equally against stderr.

## Non-goals

- No changes to flock behavior. Principle 12's mutual-exclusion contract
  is untouched.
- No changes to `add` or `recover` wrapper paths. The deleted block is
  unlock-only.
- No changes to the post-exec fixup `mountpoint -q` at
  `braid-wrapper.sh:84`. Different check (post-mount gate for
  chown/online-service), stays.
- No changes to message text. The CLI already prints exactly the same
  string the wrapper printed.
- No mention of `plan_open_pool` or `exec` in the architecture docs. Doc
  wording stays at the contract level.
