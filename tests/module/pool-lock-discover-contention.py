# Test: pool-lock-discover-contention
#
# Intent: When another process holds /run/braid-pool.lock, both
# `braid discover --write` and bare `braid discover` must fail fast before
# scanning devices or writing pool.json.
#
# Why it exists: `discover --write` has a scan-to-pool.json-write race window,
# and this test pins the agreed scope that both `--write` and bare `discover`
# invocations are serialized by the wrapper lock.
#
# Scenario: Admin holds the pool operation lock with one recovery action, then
# runs `braid discover --write` or diagnostic `braid discover` from another
# shell against a host with a discoverable braid-labeled LUKS disk. Both
# commands should fail at the wrapper lock and leave pool.json absent.

start_all()
machine.wait_for_unit("multi-user.target", timeout=120)

with subtest("Precondition: discoverable disk with no pool.json"):
    machine.succeed("test ! -e /var/lib/braid/pool.json")
    machine.succeed("test -L /dev/disk/by-id/virtio-disk1")

with subtest("braid discover fails fast when pool lock is held"):
    holder_pid = machine.succeed(
        "rm -f /tmp/holder.ready; "
        "nohup flock -x -o /run/braid-pool.lock "
        "sh -c 'touch /tmp/holder.ready; sleep 60' "
        ">/dev/null 2>&1 & echo $!"
    ).strip()
    machine.wait_until_succeeds("test -e /tmp/holder.ready", timeout=10)
    locks = machine.succeed("cat /proc/locks")
    assert "FLOCK" in locks, "no flock in /proc/locks: " + locks

    rc, out = machine.execute("timeout 5 braid discover --write 2>&1")
    assert rc != 0, "expected discover --write to fail; out=" + out
    assert rc != 124, "discover --write hung past 5s cap; out=" + out
    assert "another braid operation is already in progress" in out, (
        "expected contention message; out=" + out
    )
    machine.fail("test -e /var/lib/braid/pool.json")

    rc, out = machine.execute("timeout 5 braid discover 2>&1")
    assert rc != 0, "expected discover to fail; out=" + out
    assert rc != 124, "discover hung past 5s cap; out=" + out
    assert "another braid operation is already in progress" in out, (
        "expected contention message; out=" + out
    )

    machine.execute(f"kill {holder_pid} 2>/dev/null || true")
    machine.wait_until_succeeds("flock -n /run/braid-pool.lock true", timeout=10)

with subtest("discover succeeds after lock release"):
    machine.succeed("braid discover --write")
    machine.succeed("test -e /var/lib/braid/pool.json")
