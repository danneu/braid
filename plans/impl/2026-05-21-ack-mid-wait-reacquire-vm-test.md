# Plan: VM-level positive bounded-wait coverage for `braid ack`

## Context

`tests/module/alert-state-lock.py` covers the negative bounded-wait path
for `braid ack` (lock held forever -> ack times out and fails) and the
post-release path (lock already released -> ack succeeds). It does not
cover the positive bounded-wait path: ack is actively waiting on the
lock when the holder releases, observes the release mid-wait, and
proceeds.

The Rust unit test
`acquire_with_timeout_polls_then_succeeds_after_holder_release` at
`cli/src/pool_lock.rs:283-308` already covers this at the unit level.
The unique value of a VM analogue is end-to-end coverage of the
mid-wait re-acquire seam: a regression that leaves `poll_acquire`'s
loop intact but breaks the retry shape (e.g. a polling loop that sleeps
the full timeout before its single re-attempt) would not be caught by
either the existing held-forever subtest (which only asserts elapsed
9-14 s and rc=1, both of which the broken shape still satisfies) or by
the post-release `braid ack` at the tail of that subtest (which runs
after the holder is already gone).

`Ack`'s `LockPolicy::Timeout(10s)` wiring at `cli/src/main.rs:161` is
already pinned by `lock_policy_classifies_every_command_and_branch` at
`cli/src/main.rs:1310`, so policy-classification regressions are out of
scope for this test.

This is a low-severity testing fix in a single existing file. No
production code changes.

## Files to modify

- `tests/module/alert-state-lock.py` -- add two helpers and one new
  `with subtest(...)` block.

No `flake.nix` registration change: `alert-state-lock` is already wired
in `flake.nix` under `checks`.

## Design choice: release-file handshake, not fixed-duration sleep

A previous draft of this plan parameterized `start_lock_holder` with
`hold_seconds=3` and asserted `elapsed >= 3 && elapsed <= 8` on the
foreground `braid ack`. That lower bound is unsafe: `wait_until_succeeds`
polls until `/tmp/holder.ready` exists, which means the holder's `sleep`
has already been running for some unknown fraction of a second by the
time Python records `start = time.monotonic()`. A correctly functioning
ack can legitimately return in under 3 s on a slow VM, producing a
flaky failure on healthy code.

Instead, the new subtest uses an explicit release-file handoff:

1. The holder script flocks and blocks until `/tmp/holder.release`
   appears, instead of sleeping for a fixed duration.
2. The test backgrounds `braid ack` through a wrapper that touches
   `/tmp/ack.started` immediately before invoking `braid ack`, and
   waits for that sentinel before doing anything else. This
   synchronizes on the wrapper actually reaching the invocation point,
   not on the shell having been backgrounded. After the sentinel and
   a short post-sentinel pause for ack's own startup, the test asserts
   via a state check (`test -e /tmp/ack.done`) that ack has not
   completed while the holder is still active. This proves ack was
   waiting -- no clock involved.
3. The test then touches `/tmp/holder.release` and measures the time
   from release to ack completion. This is the assertion that catches
   the mid-wait re-acquire regression -- it is bounded by ack's poll
   interval (250 ms) plus its own work, so a generous upper bound
   tolerates VM jitter while still failing a regression that would
   force ack to wait out the remaining timeout window.

## Implementation

### 1. Add release-file helpers next to the existing holder helpers

Leave `start_lock_holder` and `stop_lock_holder` untouched -- their
existing 60 s callers (`monitor skips silently`, `ack waits then
fails`, `remove`, `remove-missing`, `add`) keep their semantics. Add
two new helpers below them:

```python
def start_lock_holder_until_release(release_path="/tmp/holder.release"):
    machine.succeed(f"rm -f /tmp/holder.ready {quote(release_path)}")
    holder_pid = machine.succeed(
        "nohup sh -c 'exec 9>/run/braid-pool.lock; "
        f"flock -x 9; touch /tmp/holder.ready; "
        f"while [ ! -e {quote(release_path)} ]; do sleep 0.1; done' "
        ">/dev/null 2>&1 & echo $!"
    ).strip()
    machine.wait_until_succeeds("test -e /tmp/holder.ready", timeout=10)
    locks = machine.succeed("cat /proc/locks")
    assert "FLOCK" in locks, (
        "no flock in /proc/locks after holder readiness signal:\n"
        f"{locks}"
    )
    return holder_pid


def release_lock_holder(holder_pid, release_path="/tmp/holder.release"):
    machine.succeed(f"touch {quote(release_path)}")
    machine.execute(f"kill {quote(holder_pid)} 2>/dev/null || true")
    machine.execute(f"rm -f /tmp/holder.ready {quote(release_path)}")
```

`release_lock_holder` is the cleanup hook: it touches the release file
(so a correctly-running holder exits naturally), then best-effort
`kill`s the PID in case the test failed before the touch, and finally
tidies state files. Mirrors the `kill || true` pattern in
`stop_lock_holder`.

### 2. Add new subtest after the existing ack contention subtest

Place it after the current `with subtest("ack waits then fails ...")`
block (ends at line 253) and before `with subtest("remove fails fast
...")` (starts at line 262), so the two ack subtests sit adjacent as
the two halves of the bounded-wait contract.

The new subtest follows the project's Test Conventions preamble
(Intent / Why it exists / Scenario).

```python
# Intent: ack waits while the pool lock is held, observes the holder's
# release mid-wait, re-acquires the lock within one poll interval, then
# clears the latch and stops the alert unit normally.
# Why it exists: protects the positive bounded-wait path at the VM
# seam. The existing ack contention subtest only covers the
# held-forever expiry path; its trailing `braid ack` runs after the
# holder is already stopped, so a regression in poll_acquire's retry
# shape -- e.g. a loop that sleeps the full timeout before its single
# re-attempt -- would still pass that subtest (elapsed ~10 s, rc=1)
# and still pass the post-release `braid ack` (lock is free). Mirrors
# `acquire_with_timeout_polls_then_succeeds_after_holder_release` in
# cli/src/pool_lock.rs at the integration seam.
# Scenario: a concurrent braid operation is holding the pool lock when
# the user runs `braid ack`. ack enters its bounded wait, and when the
# concurrent operation finishes ack should re-acquire promptly and
# ack normally -- not wait out the full 10 s timeout.
with subtest("ack re-acquires promptly when holder releases mid-wait"):
    write_missing_latch(1)
    write_acked_stats({"1": acked_disk(False, 23)})
    machine.succeed("systemctl start braid-alert.service")
    machine.succeed(
        "rm -f /tmp/ack.started /tmp/ack.done /tmp/ack.rc /tmp/ack.out"
    )

    holder_pid = start_lock_holder_until_release()
    try:
        # `touch /tmp/ack.started` lives in the wrapper immediately
        # before `braid ack` so we can synchronize on the wrapper
        # actually reaching the invocation point, rather than on the
        # shell having been backgrounded. Without this, a slow or
        # paused VM could let the test release the holder before
        # ack ever entered the lock path -- the test would then pass
        # without exercising mid-wait re-acquire.
        machine.succeed(
            "nohup sh -c "
            "'touch /tmp/ack.started; "
            "braid ack >/tmp/ack.out 2>&1; echo $? >/tmp/ack.rc; "
            "touch /tmp/ack.done' "
            ">/dev/null 2>&1 &"
        )
        machine.wait_until_succeeds(
            "test -e /tmp/ack.started", timeout=10
        )
        # Sentinel proves the wrapper reached `braid ack`. Give the
        # process a brief window to parse argv, clear the root gate,
        # and reach acquire_with_timeout, then prove it is blocked on
        # the held lock -- no timing assertion involved. Config load
        # for `Commands::Ack` happens *after* the lock is acquired
        # (cli/src/main.rs:489 takes the pool lock at dispatch before
        # the match arm runs), per ADR 026, so it is not part of the
        # pre-acquire startup gap.
        time.sleep(2)
        rc, _ = machine.execute("test -e /tmp/ack.done")
        assert rc != 0, "ack completed while pool lock was still held"

        # Release the holder and measure how long ack takes to finish.
        # Bounded: one poll interval (~250 ms) plus ack's own work.
        release_start = time.monotonic()
        machine.succeed("touch /tmp/holder.release")
        machine.wait_until_succeeds("test -e /tmp/ack.done", timeout=5)
        release_to_done = time.monotonic() - release_start
    finally:
        release_lock_holder(holder_pid)

    ack_rc = int(machine.succeed("cat /tmp/ack.rc").strip())
    ack_out = machine.succeed("cat /tmp/ack.out")

    assert ack_rc == 0, (
        f"expected ack success after holder release, got rc={ack_rc}; "
        f"out={ack_out}"
    )
    assert release_to_done <= 5, (
        f"ack did not re-acquire promptly after release; "
        f"release_to_done={release_to_done:.2f}s; out={ack_out}"
    )
    machine.fail(f"test -e {quote(alert_latch_path)}")
    machine.fail("systemctl is-active --quiet braid-alert.service")
```

The `release_to_done <= 5` assertion is the substantive guard. A
healthy ack completes in well under a second after release (poll
interval is 250 ms in `POOL_POLL_INTERVAL` at `cli/src/pool_lock.rs:22`,
plus latch clear and `systemctl stop`). A regression that forces ack
to wait the rest of the 10 s window before retrying would see
`release_to_done` around 8 s, well over the 5 s bound. The 5 s
threshold leaves several seconds of headroom for VM jitter on the
happy path.

`machine.wait_until_succeeds(..., timeout=5)` will itself raise if
`/tmp/ack.done` does not appear within 5 s, so the bound is enforced
twice -- once by the framework's wait, once by the explicit
`release_to_done` assertion that runs on success. The explicit
assertion is kept so the failure message is informative when a near-miss
regression makes ack just barely beat the framework timeout.

## Verification

End-to-end:

```
just test-vm alert-state-lock
```

Regression-check the new assertion (run locally, do not commit):

1. In `cli/src/pool_lock.rs`, mutate `poll_acquire` so that on
   contention it sleeps the entire remaining timeout window before
   making a single re-acquire attempt -- i.e. replace the polling loop
   body with `thread::sleep(timeout); self.try_acquire()`. This
   preserves the held-forever expiry path's elapsed window (~10 s,
   rc=1, contention message) and the post-release ack at the tail of
   the existing subtest (lock is free, single try succeeds), so both
   are expected to keep passing.
2. `just test-vm alert-state-lock` -- the new subtest must fail:
   either `wait_until_succeeds` times out (because ack is still
   sleeping when the test's 5 s wait window expires), or
   `release_to_done` is greater than 5.
3. Revert the production mutation; rerun to confirm the new subtest
   passes again.

Rust side is already covered:

```
just test-rust
```

remains green via `acquire_with_timeout_polls_then_succeeds_after_holder_release`.

No fixture refresh required (no parser-critical tool versions change).
