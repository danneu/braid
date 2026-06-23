# Test: recover from interrupted replace (crash before replace started)
#
# Intent: Verify `braid recover` correctly rebuilds pool.json from live btrfs
# topology when a Replace journal exists but the replace operation never
# started. The pool still has the original 3 disks; the replacement disk has
# no LUKS container.
#
# Why it exists: The existing braid-recover test only covers Add journals.
# Replace journals create a union of pre + target memberships that includes
# both old and new devices. The recover code must correctly match live btrfs
# devices against this union to resolve by_id paths. A bug here could write
# a corrupt pool.json entry (wrong by_id), breaking subsequent unlock/lock.
#
# Scenario: 3-disk RAID1 pool. Operator starts `braid replace disk2 disk4`
# but the system crashes after writing the journal and before any disk
# operations. On reboot, `braid recover` must discover the pool still has
# {disk1, disk2, disk3} and write pool.json accordingly.

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


def members_except(pool, *names):
    skip = set(names)
    return {
        uuid: member
        for uuid, member in pool["disks"].items()
        if member["name"] not in skip
    }


# --- Phase 1: Build 3-disk RAID1 pool and write test data ---

with subtest("Build 3-disk pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed(add_cmd("disk3"))
    machine.succeed("mountpoint -q /mnt/storage")

    machine.succeed("echo 'replace-recover-data' > /mnt/storage/testfile.txt")
    machine.succeed("sync")

    pool_json = json.loads(machine.succeed("cat /var/lib/braid/pool.json"))

# --- Phase 2: Lock pool and inject Replace journal ---

with subtest("Lock pool and inject Replace journal"):
    machine.succeed("braid lock")
    machine.fail("mountpoint -q /mnt/storage")

    old_uuid, old_member = member_entry(pool_json, "disk2")
    new_uuid = "44444444-4444-4444-4444-444444444444"

    # Build target_membership: disk2 removed, disk4 added
    target_disks = members_except(pool_json, "disk2")
    target_disks[new_uuid] = {
        "name": "disk4",
        "by_id": "/dev/disk/by-id/virtio-disk4",
    }
    target_json = {"disks": target_disks}

    journal = {
        "started_at": "2026-01-01T00:00:00Z",
        "op": {
            "op": "Replace",
            "phase": "PoolMutation",
            "old_uuid": old_uuid,
            "old_name": "disk2",
            "new_uuid": new_uuid,
            "new_name": "disk4",
            "new_target": {
                "by_id": "/dev/disk/by-id/virtio-disk4",
                "mode": {
                    "FreshLuks": {
                        "extra_opts": [
                            "--pbkdf",
                            "pbkdf2",
                            "--pbkdf-force-iterations",
                            "1000",
                        ],
                        "enroll_key_file": None,
                    }
                },
            },
            "source": {
                "Live": {
                    "old_devid": old_member["devid"],
                    "old_mapper": "braid-disk2",
                }
            },
            "restore_raid1_after_commit": False,
        },
        "pre_membership": pool_json,
        "target_membership": target_json,
    }
    journal_str = json.dumps(journal)
    machine.succeed(
        f"cat > /var/lib/braid/pending-op.json << 'JOURNAL_EOF'\n"
        f"{journal_str}\n"
        f"JOURNAL_EOF"
    )
    machine.succeed("test -f /var/lib/braid/pending-op.json")

# --- Phase 3: braid unlock must refuse ---

with subtest("braid unlock refuses with journal present"):
    exit_code, output = machine.execute(
        f"printf '%s\\n' {pq} | braid unlock --passphrase-stdin 2>&1"
    )
    assert exit_code != 0, f"unlock should fail, but got exit {exit_code}"
    assert "interrupted operation" in output, (
        f"Expected 'interrupted operation' in output, got: {output}"
    )

# --- Phase 4: braid recover rebuilds from live state ---

with subtest("braid recover rebuilds pool.json from live pool"):
    machine.succeed(
        f"printf '%s\\n' {pq} | braid recover --passphrase-stdin --allow-degraded"
    )
    machine.succeed("mountpoint -q /mnt/storage")

    recovered = json.loads(machine.succeed("cat /var/lib/braid/pool.json"))

    # Must contain disk1, disk2, disk3 (the actual pool members)
    for name in ["disk1", "disk2", "disk3"]:
        _, recovered_member = member_entry(recovered, name)
        expected_by_id = f"/dev/disk/by-id/virtio-{name}"
        actual_by_id = recovered_member["by_id"]
        assert actual_by_id == expected_by_id, (
            f"{name} by_id mismatch: expected {expected_by_id}, got {actual_by_id}"
        )

    # Must NOT contain disk4 (replace never happened)
    assert all(member["name"] != "disk4" for member in recovered["disks"].values()), (
        f"disk4 should not be in recovered pool.json: {recovered}"
    )

    # Journal must be cleared
    machine.fail("test -f /var/lib/braid/pending-op.json")

# --- Phase 5: Data intact ---

with subtest("Test data intact after recovery"):
    content = machine.succeed("cat /mnt/storage/testfile.txt").strip()
    assert content == "replace-recover-data", (
        f"Expected 'replace-recover-data', got: {content}"
    )

# --- Phase 6: Normal operations resume ---

with subtest("Normal operations resume after recovery"):
    machine.succeed("braid lock")
    machine.fail("mountpoint -q /mnt/storage")

    machine.succeed(
        f"printf '%s\\n' {pq} | braid unlock --passphrase-stdin"
    )
    machine.succeed("mountpoint -q /mnt/storage")

    content = machine.succeed("cat /mnt/storage/testfile.txt").strip()
    assert content == "replace-recover-data", (
        f"Expected 'replace-recover-data', got: {content}"
    )

machine.shutdown()
