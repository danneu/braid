# Test: braid recover from interrupted remove
#
# Intent: Verify `braid recover` correctly rebuilds pool.json after a crash
# interrupts a `braid remove` operation, at both crash timing points.
#
# Why it exists: The existing braid-recover test only covers OpKind::Add.
# Remove recovery has a unique subtlety: the removed disk's LUKS container
# still exists on the physical device, so union_memberships opens it during
# recovery, but probe_pool must correctly exclude it from the rebuilt
# membership. A regression in the topology probe could leave a removed device
# in pool.json, causing braid unlock to try opening a disk that's no longer
# in the pool.
#
# Scenario:
#   A) Crash before btrfs device remove: 3-disk RAID1 pool, journal injected,
#      btrfs still has all 3 devices. Recovery sees 3 → rebuilds with 3.
#   B) Crash after btrfs device remove but before pool.json write: disk3 is
#      evicted from btrfs but its LUKS container still exists. Recovery opens
#      all 3 LUKS devices (via union membership) but probes btrfs → sees only
#      2 devices → rebuilds pool.json with disk1+disk2, while braid-disk3
#      mapper remains open.

import json
import shlex

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
pq = shlex.quote(passphrase)
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def add_cmd(key):
    return (
        f"printf '%s\\n' {pq} | "
        f"braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 {key}=/dev/disk/by-id/virtio-{key} --passphrase-stdin --yes"
    )


def build_remove_journal(pool_json):
    """Build a pending-op.json for an interrupted remove of disk3."""
    target = json.loads(json.dumps(pool_json))
    del target["disks"]["disk3"]
    return {
        "started_at": "2026-01-01T00:00:00Z",
        "op": {
            "op": "Remove",
            "name": "disk3",
        },
        "pre_membership": pool_json,
        "target_membership": target,
    }


def inject_journal(journal):
    """Write pending-op.json to the braid state directory."""
    journal_json = json.dumps(journal)
    machine.succeed(
        f"cat > /var/lib/braid/pending-op.json << 'JOURNAL_EOF'\n"
        f"{journal_json}\n"
        f"JOURNAL_EOF"
    )
    machine.succeed("test -f /var/lib/braid/pending-op.json")


def build_pool():
    """Build a 3-disk RAID1 pool and write test data."""
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed(add_cmd("disk3"))
    machine.succeed("mountpoint -q /mnt/storage")
    machine.succeed("echo 'recovery-test-data' > /mnt/storage/testfile.txt")
    machine.succeed("sync")
    return json.loads(machine.succeed("cat /var/lib/braid/pool.json"))


def teardown_pool():
    """Fully tear down the pool: lock, delete state, wipe disk headers."""
    machine.succeed("braid lock")
    machine.succeed("rm -f /var/lib/braid/pool.json")
    for disk in ["disk1", "disk2", "disk3"]:
        machine.succeed(
            f"dd if=/dev/zero of=/dev/disk/by-id/virtio-{disk} bs=1M count=4"
        )


# ============================================================
# Scenario A: Crash BEFORE btrfs device remove
# ============================================================
# btrfs still has all 3 devices. Recovery probes topology, sees 3
# devices, and rebuilds pool.json with all 3.

with subtest("A: Build 3-disk pool"):
    pool_json_a = build_pool()

with subtest("A: Lock pool and inject remove journal"):
    machine.succeed("braid lock")
    machine.fail("mountpoint -q /mnt/storage")
    inject_journal(build_remove_journal(pool_json_a))

with subtest("A: braid unlock refuses with journal present"):
    exit_code, output = machine.execute(
        f"printf '%s\\n' {pq} | braid unlock --passphrase-stdin 2>&1"
    )
    assert exit_code != 0, f"unlock should fail, but got exit {exit_code}"
    assert "interrupted operation" in output, (
        f"Expected 'interrupted operation' in output, got: {output}"
    )

with subtest("A: braid recover rebuilds pool.json with all 3 disks"):
    machine.succeed(
        f"printf '%s\\n' {pq} | braid recover --passphrase-stdin"
    )
    machine.succeed("mountpoint -q /mnt/storage")

    recovered = json.loads(machine.succeed("cat /var/lib/braid/pool.json"))
    for name in ["disk1", "disk2", "disk3"]:
        assert name in recovered["disks"], (
            f"{name} missing from recovered pool.json: {recovered}"
        )

    machine.fail("test -f /var/lib/braid/pending-op.json")

with subtest("A: Test data intact"):
    content = machine.succeed("cat /mnt/storage/testfile.txt").strip()
    assert content == "recovery-test-data", (
        f"Expected 'recovery-test-data', got: {content}"
    )

with subtest("A: Normal operations resume"):
    machine.succeed("braid lock")
    machine.fail("mountpoint -q /mnt/storage")
    machine.succeed(
        f"printf '%s\\n' {pq} | braid unlock --passphrase-stdin"
    )
    machine.succeed("mountpoint -q /mnt/storage")
    content = machine.succeed("cat /mnt/storage/testfile.txt").strip()
    assert content == "recovery-test-data", (
        f"Expected 'recovery-test-data', got: {content}"
    )

# ============================================================
# Teardown + rebuild for Scenario B
# ============================================================

with subtest("Teardown and rebuild fresh pool for Scenario B"):
    teardown_pool()
    pool_json_b = build_pool()

# ============================================================
# Scenario B: Crash AFTER btrfs device remove
# ============================================================
# disk3 has been evicted from btrfs but pool.json was not updated
# (simulating a crash between the btrfs op and the pool.json write).
# disk3's LUKS container still exists on the physical device.
# Recovery opens all 3 LUKS devices via union_memberships, probes
# btrfs → sees 2 devices → rebuilds pool.json with disk1+disk2.

with subtest("B: Manually evict disk3 from btrfs"):
    machine.succeed(
        "btrfs device remove /dev/mapper/braid-disk3 /mnt/storage"
    )
    machine.succeed("cryptsetup close braid-disk3")

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "braid-disk3" not in fi_show, (
        f"disk3 should be gone from btrfs:\n{fi_show}"
    )
    for name in ["braid-disk1", "braid-disk2"]:
        assert f"/dev/mapper/{name}" in fi_show, (
            f"{name} missing from btrfs:\n{fi_show}"
        )

with subtest("B: Lock pool and inject remove journal"):
    machine.succeed("braid lock")
    machine.fail("mountpoint -q /mnt/storage")
    inject_journal(build_remove_journal(pool_json_b))

with subtest("B: braid unlock refuses with journal present"):
    exit_code, output = machine.execute(
        f"printf '%s\\n' {pq} | braid unlock --passphrase-stdin 2>&1"
    )
    assert exit_code != 0, f"unlock should fail, but got exit {exit_code}"
    assert "interrupted operation" in output, (
        f"Expected 'interrupted operation' in output, got: {output}"
    )

with subtest("B: braid recover rebuilds pool.json with disk1+disk2 only"):
    machine.succeed(
        f"printf '%s\\n' {pq} | braid recover --passphrase-stdin"
    )
    machine.succeed("mountpoint -q /mnt/storage")

    recovered = json.loads(machine.succeed("cat /var/lib/braid/pool.json"))
    assert "disk1" in recovered["disks"], (
        f"disk1 missing from recovered pool.json: {recovered}"
    )
    assert "disk2" in recovered["disks"], (
        f"disk2 missing from recovered pool.json: {recovered}"
    )
    assert "disk3" not in recovered["disks"], (
        f"disk3 should NOT be in recovered pool.json: {recovered}"
    )

    machine.fail("test -f /var/lib/braid/pending-op.json")

with subtest("B: Recovery opened disk3 LUKS despite excluding it from membership"):
    # This is the key edge case: union_memberships includes disk3 (it's in
    # pre_membership), so recovery opens its LUKS container. But probe_pool
    # sees disk3 is no longer in btrfs and excludes it from pool.json.
    # The mapper must still be open — proving recovery tolerates an
    # openable-but-no-longer-member disk.
    machine.succeed("test -e /dev/mapper/braid-disk3")

with subtest("B: Test data intact"):
    content = machine.succeed("cat /mnt/storage/testfile.txt").strip()
    assert content == "recovery-test-data", (
        f"Expected 'recovery-test-data', got: {content}"
    )

with subtest("B: Normal operations resume"):
    machine.succeed("braid lock")
    machine.fail("mountpoint -q /mnt/storage")
    machine.succeed(
        f"printf '%s\\n' {pq} | braid unlock --passphrase-stdin"
    )
    machine.succeed("mountpoint -q /mnt/storage")
    content = machine.succeed("cat /mnt/storage/testfile.txt").strip()
    assert content == "recovery-test-data", (
        f"Expected 'recovery-test-data', got: {content}"
    )

machine.shutdown()
