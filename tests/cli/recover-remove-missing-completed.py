# Test: recover from interrupted remove-missing after btrfs commit
#
# Intent: Verify `braid recover` completes a RemoveMissing::PoolMutation
# journal after btrfs has already removed the missing devid.
#
# Why it exists: Unit tests cover the dispatcher with mocked pool states, but
# this pins the VM integration path: degraded mount, real btrfs missing-device
# probing, UUID-keyed membership resolution with by-id re-resolved from the
# live backing device, and journal cleanup.
#
# Scenario: 3-disk RAID1 pool. disk3 disappears, btrfs `device remove missing`
# succeeds, and the system crashes before braid can rewrite pool.json and clear
# pending-op.json. On reboot, recovery must write disk1+disk2 membership and
# finish post-remove-missing maintenance.

import json
import shlex

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
pq = shlex.quote(passphrase)


def add_cmd(key):
    return (
        f"printf '%s\\n' {pq} | "
        f"braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 "
        f"--luks-format-arg=--pbkdf-force-iterations "
        f"--luks-format-arg=1000 {key}=/dev/disk/by-id/virtio-{key} "
        f"--passphrase-stdin --yes"
    )


def read_pool():
    return json.loads(machine.succeed("cat /var/lib/braid/pool.json"))


def member_entry(pool, name):
    for uuid, member in pool["disks"].items():
        if member["name"] == name:
            return uuid, member
    raise AssertionError(f"{name} missing from pool.json: {pool}")


def members_except(pool, *names):
    skip = set(names)
    return {
        uuid: member
        for uuid, member in pool["disks"].items()
        if member["name"] not in skip
    }


def has_member(pool, name):
    return any(member["name"] == name for member in pool["disks"].values())


def missing_devid():
    raw = machine.succeed("braid status --json")
    report = json.loads(raw)
    devids = report.get("missing_devids", [])
    assert len(devids) == 1, f"expected one missing devid, got {devids}:\n{raw}"
    return devids[0]


def inject_remove_missing_journal(pre_pool, devid):
    target_pool = {"disks": members_except(pre_pool, "disk3")}
    journal = {
        "started_at": "2026-01-01T00:00:00Z",
        "op": {
            "op": "RemoveMissing",
            "phase": "PoolMutation",
            "devid": devid,
            "restore_raid1_after_commit": True,
        },
        "pre_membership": pre_pool,
        "target_membership": target_pool,
    }
    journal_str = json.dumps(journal)
    machine.succeed(
        f"cat > /var/lib/braid/pending-op.json << 'JOURNAL_EOF'\n"
        f"{journal_str}\n"
        f"JOURNAL_EOF"
    )
    machine.succeed("test -f /var/lib/braid/pending-op.json")


with subtest("Build 3-disk pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed(add_cmd("disk3"))
    machine.succeed("mountpoint -q /mnt/storage")
    machine.succeed("echo 'remove-missing-recover-data' > /mnt/storage/testfile.txt")
    machine.succeed("sync")

    pre_pool_json = read_pool()

with subtest("Simulate disk3 death and mount degraded"):
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup close braid-disk3")
    machine.succeed("mount -o degraded /dev/mapper/braid-disk1 /mnt/storage")

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "missing" in fi_show.lower(), f"expected missing device:\n{fi_show}"
    removed_devid = missing_devid()

with subtest("Commit btrfs remove missing outside braid"):
    machine.succeed("btrfs device remove --enqueue missing /mnt/storage")

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "missing" not in fi_show.lower(), (
        f"missing device should be gone after btrfs remove missing:\n{fi_show}"
    )
    assert "braid-disk3" not in fi_show, f"disk3 still in pool:\n{fi_show}"
    for name in ["braid-disk1", "braid-disk2"]:
        assert f"/dev/mapper/{name}" in fi_show, f"{name} missing:\n{fi_show}"

with subtest("Lock pool and inject RemoveMissing journal"):
    machine.succeed("braid lock")
    machine.fail("mountpoint -q /mnt/storage")

    pre_pool_str = json.dumps(pre_pool_json)
    machine.succeed(
        f"cat > /var/lib/braid/pool.json << 'POOL_EOF'\n"
        f"{pre_pool_str}\n"
        f"POOL_EOF"
    )
    inject_remove_missing_journal(pre_pool_json, removed_devid)

with subtest("braid unlock refuses with journal present"):
    exit_code, output = machine.execute(
        f"printf '%s\\n' {pq} | braid unlock --passphrase-stdin 2>&1"
    )
    assert exit_code != 0, f"unlock should fail, but got exit {exit_code}"
    assert "interrupted operation" in output, (
        f"expected interrupted-operation refusal, got:\n{output}"
    )

with subtest("braid recover completes committed remove-missing"):
    exit_code, output = machine.execute(
        f"printf '%s\\n' {pq} | braid recover --passphrase-stdin 2>&1"
    )
    print(f"=== braid recover output ===\n{output}")
    assert exit_code == 0, f"recover failed with exit {exit_code}:\n{output}"
    assert "pool.json written from committed remove-missing membership" in output, (
        f"recover did not take the committed remove-missing path:\n{output}"
    )

    machine.succeed("mountpoint -q /mnt/storage")
    machine.fail("test -f /var/lib/braid/pending-op.json")

with subtest("pool.json reflects disk1+disk2 only"):
    recovered = read_pool()
    for name in ["disk1", "disk2"]:
        assert has_member(recovered, name), (
            f"{name} missing from recovered pool.json: {recovered}"
        )
        _, recovered_member = member_entry(recovered, name)
        expected_by_id = f"/dev/disk/by-id/virtio-{name}"
        actual_by_id = recovered_member["by_id"]
        assert actual_by_id == expected_by_id, (
            f"{name} by_id mismatch: expected {expected_by_id}, got {actual_by_id}"
        )
    assert not has_member(recovered, "disk3"), (
        f"disk3 should not be in recovered pool.json: {recovered}"
    )

with subtest("Live pool has no missing device"):
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "missing" not in fi_show.lower(), (
        f"missing device still present after recover:\n{fi_show}"
    )
    assert "braid-disk3" not in fi_show, f"disk3 still in live pool:\n{fi_show}"

with subtest("Test data intact after recovery"):
    content = machine.succeed("cat /mnt/storage/testfile.txt").strip()
    assert content == "remove-missing-recover-data", (
        f"expected test data after recover, got {content!r}"
    )

with subtest("Normal operations resume after recovery"):
    machine.succeed("braid lock")
    machine.fail("mountpoint -q /mnt/storage")
    machine.succeed(f"printf '%s\\n' {pq} | braid unlock --passphrase-stdin")
    machine.succeed("mountpoint -q /mnt/storage")
    content = machine.succeed("cat /mnt/storage/testfile.txt").strip()
    assert content == "remove-missing-recover-data", (
        f"expected test data after unlock, got {content!r}"
    )

machine.shutdown()
