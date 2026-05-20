# Test: pool-lock-contention
#
# Intent: When another process holds /run/braid-pool.lock, Rust dispatch
# must fail fast (exit 1) with a clear "another braid operation is
# already in progress" message — never hang.
#
# Why it exists: Without -n on the flock call, a wedged holder (e.g. a
# long-running `braid add` balance) would silently hang any concurrent
# `braid unlock` invocation forever, with no feedback. A blocking-flock
# regression must fail this test, not silently pass it.
# See docs/decisions/026-pool-lock-rust-owned.md.
#
# Scenario: Admin starts `braid add` in one shell (modeled here as a
# background flock holder), then opens a second shell and runs
# `braid unlock` — the second invocation should fail immediately.

import shlex

start_all()
machine.wait_for_unit("multi-user.target", timeout=120)

passphrase = "testpassphrase"
pq = shlex.quote(passphrase)

with subtest("Precondition: pool offline"):
    machine.fail("mountpoint -q /mnt/storage")

with subtest("braid unlock fails fast when pool lock is held"):
    # Start the holder. The holder ONLY writes /tmp/holder.ready AFTER
    # flock has actually acquired the lock — without this readiness
    # signal the test would race the holder and could falsely take the
    # normal unlock path before the lock is held.
    #
    # `nohup ... >/dev/null 2>&1 & echo $!` is the canonical pattern
    # for backgrounding a long-lived process under the test runner —
    # without redirecting stdio the SSH session waits on inherited FDs
    # and hangs until the sleep completes. See tests/cli/braid-lock-
    # umount-busy.py for the same pattern.
    #
    # `flock FILE COMMAND` is the file-form of flock: it opens the
    # path itself, acquires an exclusive lock, then runs COMMAND with
    # the lock held. Simpler than redirecting FD 9 by hand.
    holder_pid = machine.succeed(
        "rm -f /tmp/holder.ready; "
        "nohup flock -x /run/braid-pool.lock "
        "sh -c 'touch /tmp/holder.ready; sleep 60' "
        ">/dev/null 2>&1 & echo $!"
    ).strip()
    # Block until the holder confirms it actually owns the lock.
    machine.wait_until_succeeds("test -e /tmp/holder.ready", timeout=10)
    # Independently verify the kernel sees an active flock on the
    # lock file before we try contending — defense in depth against
    # an unreliable readiness signal.
    locks = machine.succeed("cat /proc/locks")
    assert "FLOCK" in locks, (
        "no flock in /proc/locks after holder readiness signal:\n{}".format(locks)
    )

    # Wall-clock cap of 5s — non-blocking flock should fail in well
    # under a second. The cap exists so the test fails (not hangs) if
    # Rust dispatch regresses to blocking flock.
    rc, out = machine.execute(
        f"timeout 5 sh -c 'printf %s\\n {pq} | "
        f"braid unlock --passphrase-stdin' 2>&1"
    )

    # Always tear down the holder before asserting, so a failed
    # assertion doesn't leave the lock held into the next subtest.
    machine.execute("kill {} 2>/dev/null || true".format(holder_pid))

    assert rc != 0, "expected unlock to fail, got rc=0; out={}".format(out)
    assert rc != 124, (
        "unlock hung past 5s wall-clock cap -- Rust dispatch "
        "regressed to blocking flock; out={}".format(out)
    )
    assert "another braid operation is already in progress" in out, (
        "expected contention message; out={}".format(out)
    )

    # The contention failure must not have mounted the pool.
    machine.fail("mountpoint -q /mnt/storage")
