# Test: pool-lock-enroll-contention
#
# Intent:
#   `braid enroll` must fail fast when /run/braid-pool.lock is already held.
#
# Why it exists:
#   Enrollment mutates LUKS keyslots across pool members. It must not run
#   concurrently with pool topology changes or recovery.
#
# Scenario:
#   Admin starts a long-running pool mutation, then tries to enroll a keyfile
#   from another shell. The enrollment command should stop at lock acquisition
#   before reading membership or touching keyslots.

start_all()
machine.wait_for_unit("multi-user.target", timeout=120)

with subtest("braid enroll fails fast when pool lock is held"):
    hold_secs = 4
    machine.succeed(
        "rm -f /tmp/holder.ready; "
        "nohup flock -x /run/braid-pool.lock "
        f"sh -c 'touch /tmp/holder.ready; sleep {hold_secs}' "
        ">/dev/null 2>&1 &"
    )
    machine.wait_until_succeeds("test -e /tmp/holder.ready", timeout=10)

    rc, out = machine.execute(
        "timeout 5 sh -c 'printf x | braid enroll /nonexistent --passphrase-stdin' 2>&1"
    )

    machine.wait_until_succeeds(
        "flock -n /run/braid-pool.lock true", timeout=hold_secs + 5
    )

    assert rc != 0, "expected enroll to fail; out=" + out
    assert rc != 124, "enroll hung past 5s cap; out=" + out
    assert "another braid operation is already in progress" in out, (
        "expected contention message; out=" + out
    )
    assert "No such file or directory" not in out, (
        "enroll read keyfile path before acquiring lock; out=" + out
    )
