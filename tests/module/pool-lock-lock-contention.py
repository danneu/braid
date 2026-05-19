# Test: pool-lock-lock-contention
#
# Intent:
#   `braid lock` must fail fast when /run/braid-pool.lock is already held.
#
# Why it exists:
#   `lock` is a pool mutator: it unmounts btrfs and closes LUKS mappers.
#   Missing it from the serialized command set lets it race an in-flight add,
#   remove, replace, or recover operation.
#
# Scenario:
#   Admin starts a long-running pool mutation in one shell, then runs
#   `braid lock` in another. The second command should report contention
#   immediately instead of unmounting underneath the active operation.

start_all()
machine.wait_for_unit("multi-user.target", timeout=120)

with subtest("braid lock fails fast when pool lock is held"):
    hold_secs = 4
    machine.succeed(
        "rm -f /tmp/holder.ready; "
        "nohup flock -x /run/braid-pool.lock "
        f"sh -c 'touch /tmp/holder.ready; sleep {hold_secs}' "
        ">/dev/null 2>&1 &"
    )
    machine.wait_until_succeeds("test -e /tmp/holder.ready", timeout=10)

    rc, out = machine.execute("timeout 5 braid lock 2>&1")

    machine.wait_until_succeeds(
        "flock -n /run/braid-pool.lock true", timeout=hold_secs + 5
    )

    assert rc != 0, "expected lock to fail; out=" + out
    assert rc != 124, "lock hung past 5s cap; out=" + out
    assert "another braid operation is already in progress" in out, (
        "expected contention message; out=" + out
    )
