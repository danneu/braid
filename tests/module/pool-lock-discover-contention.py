# Test: pool-lock-discover-contention
#
# Intent: When another process holds /run/braid-pool.lock,
# `braid discover --write` must fail fast before scanning or writing, while
# bare read-only `braid discover` may proceed and must leave pool.json
# unchanged.
#
# Why it exists: `discover --write` has a scan-to-pool.json-write race window,
# and this test pins the agreed scope that only the writing form is serialized
# by the Rust-owned pool lock.
#
# Scenario: Admin holds the pool operation lock with one recovery action, then
# runs `braid discover --write` or diagnostic `braid discover` from another
# shell against a host with a discoverable braid-labeled LUKS disk. The writing
# command fails at the pool lock; the read-only command may scan but leaves the
# existing UUID-keyed pool.json bytes unchanged.

import json

start_all()
machine.wait_for_unit("multi-user.target", timeout=120)

with subtest("Precondition: discover writes UUID-keyed pool.json"):
    machine.succeed("test ! -e /var/lib/braid/pool.json")
    machine.succeed("test -L /dev/disk/by-id/virtio-disk1")
    machine.succeed("braid discover --write --expect-count 1")
    pool_before = machine.succeed("cat /var/lib/braid/pool.json")
    pool = json.loads(pool_before)
    assert set(pool["disks"].keys()) == {
        "11111111-1111-1111-1111-111111111111"
    }, pool_before
    assert pool["disks"]["11111111-1111-1111-1111-111111111111"]["name"] == "disk1"

with subtest("braid discover fails fast when pool lock is held"):
    hold_secs = 8
    machine.succeed(
        "rm -f /tmp/holder.ready; "
        "nohup flock -x -o /run/braid-pool.lock "
        f"sh -c 'touch /tmp/holder.ready; sleep {hold_secs}' "
        ">/dev/null 2>&1 &"
    )
    machine.wait_until_succeeds("test -e /tmp/holder.ready", timeout=10)
    locks = machine.succeed("cat /proc/locks")
    assert "FLOCK" in locks, "no flock in /proc/locks: " + locks

    rc, out = machine.execute("timeout 5 braid discover --write --expect-count 1 2>&1")
    assert rc != 0, "expected discover --write to fail; out=" + out
    assert rc != 124, "discover --write hung past 5s cap; out=" + out
    assert "another braid operation is already in progress" in out, (
        "expected contention message; out=" + out
    )
    assert machine.succeed("cat /var/lib/braid/pool.json") == pool_before

    rc, out = machine.execute("timeout 5 braid discover 2>&1")
    assert rc != 124, "discover hung past 5s cap; out=" + out
    assert "another braid operation is already in progress" not in out, (
        "bare discover should not acquire the pool lock; out=" + out
    )
    assert machine.succeed("cat /var/lib/braid/pool.json") == pool_before

    machine.wait_until_succeeds(
        "flock -n /run/braid-pool.lock true", timeout=hold_secs + 5
    )

with subtest("discover reaches CLI after lock release and refuses healthy UUID-keyed pool.json"):
    rc, out = machine.execute("braid discover --write --expect-count 1 2>&1")
    assert rc != 0, "expected ValidUuidKeyed refusal; out=" + out
    assert "another braid operation is already in progress" not in out, (
        "wrapper lock check must not fire after release; out=" + out
    )
    assert "is already a healthy UUID-keyed membership" in out, (
        "expected ValidUuidKeyed refusal at the gate; out=" + out
    )
    assert machine.succeed("cat /var/lib/braid/pool.json") == pool_before, (
        "pool.json must be byte-for-byte unchanged after refusal"
    )
