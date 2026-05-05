# Test: recover from interrupted replace (crash after replace completed)
#
# Intent: Verify `braid recover` correctly rebuilds pool.json when a btrfs
# replace completed but the system crashed before pool.json was updated.
# The live pool has the new disk (disk4); the stale pool.json still references
# the old disk (disk2). Recovery must discover disk4 in the live pool and
# resolve its by_id from the journal's target_membership union.
#
# Why it exists: This is the most dangerous replace crash state. The pool's
# actual topology diverges from what pool.json says. Recovery must probe the
# live btrfs filesystem, find disk4, and look up its by_id in the journal's
# target_membership — not the stale pool.json. A bug here (e.g. wrong by_id,
# including both old and new, or neither) would corrupt pool.json and break
# subsequent unlock/lock cycles.
#
# Scenario: 3-disk RAID1 pool. Operator runs `braid replace disk2 disk4`.
# The btrfs replace operation completes (pool now has disk1+disk3+disk4), but
# the system crashes before pool.json is written and the journal is cleared.
# On reboot, `braid recover` must write pool.json with {disk1, disk3, disk4}
# using correct by_id paths.

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


def replace_cmd(old, new):
    return (
        f"printf '%s\\n' {pq} | "
        f"braid replace --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 --old {old} --new {new}=/dev/disk/by-id/virtio-{new} "
        f"--passphrase-stdin --yes"
    )


# --- Phase 1: Build 3-disk RAID1 pool and write test data ---

with subtest("Build 3-disk pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed(add_cmd("disk3"))
    machine.succeed("mountpoint -q /mnt/storage")

    machine.succeed("echo 'replace-recover-data' > /mnt/storage/testfile.txt")
    machine.succeed("sync")

    # Capture pre-replace pool.json for later rollback
    pre_replace_json = json.loads(machine.succeed("cat /var/lib/braid/pool.json"))

# --- Phase 2: Perform real replace (disk2 → disk4) ---

with subtest("Perform real replace disk2 with disk4"):
    machine.succeed(replace_cmd("disk2", "disk4"))

    # Verify the replace actually worked
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "/dev/mapper/braid-disk4" in fi_show, (
        f"disk4 should be in pool after replace:\n{fi_show}"
    )
    assert "braid-disk2" not in fi_show, (
        f"disk2 should be removed after replace:\n{fi_show}"
    )

# --- Phase 3: Lock pool and simulate crash state ---

with subtest("Lock pool and roll back metadata to simulate crash"):
    machine.succeed("braid lock")
    machine.fail("mountpoint -q /mnt/storage")

    # Overwrite pool.json with pre-replace state (simulating crash before
    # pool.json was written after btrfs replace completed)
    pre_replace_str = json.dumps(pre_replace_json)
    machine.succeed(
        f"cat > /var/lib/braid/pool.json << 'POOL_EOF'\n"
        f"{pre_replace_str}\n"
        f"POOL_EOF"
    )

    # Build target_membership: disk2 removed, disk4 added
    target_disks = {}
    for name, member in pre_replace_json["disks"].items():
        if name != "disk2":
            target_disks[name] = member
    target_disks["disk4"] = {"by_id": "/dev/disk/by-id/virtio-disk4"}
    target_json = {"disks": target_disks}

    # Inject pending-op.json
    journal = {
        "started_at": "2026-01-01T00:00:00Z",
        "op": {
            "op": "Replace",
            "old_name": "disk2",
            "new_name": "disk4",
            "new_by_id": "/dev/disk/by-id/virtio-disk4",
        },
        "pre_membership": pre_replace_json,
        "target_membership": target_json,
    }
    journal_str = json.dumps(journal)
    machine.succeed(
        f"cat > /var/lib/braid/pending-op.json << 'JOURNAL_EOF'\n"
        f"{journal_str}\n"
        f"JOURNAL_EOF"
    )
    machine.succeed("test -f /var/lib/braid/pending-op.json")

# --- Phase 4: braid unlock must refuse ---

with subtest("braid unlock refuses with journal present"):
    exit_code, output = machine.execute(
        f"printf '%s\\n' {pq} | braid unlock --passphrase-stdin 2>&1"
    )
    assert exit_code != 0, f"unlock should fail, but got exit {exit_code}"
    assert "interrupted operation" in output, (
        f"Expected 'interrupted operation' in output, got: {output}"
    )

# --- Phase 5: braid recover rebuilds from live state ---

with subtest("braid recover rebuilds pool.json from live pool"):
    machine.succeed(
        f"printf '%s\\n' {pq} | braid recover --passphrase-stdin"
    )
    machine.succeed("mountpoint -q /mnt/storage")

    recovered = json.loads(machine.succeed("cat /var/lib/braid/pool.json"))

    # Must contain disk1, disk3, disk4 (the actual pool members after replace)
    expected_members = {
        "disk1": "/dev/disk/by-id/virtio-disk1",
        "disk3": "/dev/disk/by-id/virtio-disk3",
        "disk4": "/dev/disk/by-id/virtio-disk4",
    }
    for name, expected_by_id in expected_members.items():
        assert name in recovered["disks"], (
            f"{name} missing from recovered pool.json: {recovered}"
        )
        actual_by_id = recovered["disks"][name]["by_id"]
        assert actual_by_id == expected_by_id, (
            f"{name} by_id mismatch: expected {expected_by_id}, got {actual_by_id}"
        )

    # Must NOT contain disk2 (replaced and no longer in btrfs)
    assert "disk2" not in recovered["disks"], (
        f"disk2 should not be in recovered pool.json: {recovered}"
    )

    # Journal must be cleared
    machine.fail("test -f /var/lib/braid/pending-op.json")

# --- Phase 6: Data intact ---

with subtest("Test data intact after recovery"):
    content = machine.succeed("cat /mnt/storage/testfile.txt").strip()
    assert content == "replace-recover-data", (
        f"Expected 'replace-recover-data', got: {content}"
    )

# --- Phase 7: Normal operations resume ---

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
