# Test: braid recover
#
# Intent: Verify `braid recover` can self-mount the pool (open LUKS + mount)
# and rebuild pool.json from live state when recovering from an interrupted
# mutation.
#
# Why it exists: There was a chicken-and-egg: `braid unlock` refuses when
# pending-op.json exists, and `braid recover` required the pool to already be
# mounted. Users had to manually run cryptsetup + mount — the exact low-level
# commands braid exists to abstract away.
#
# Scenario: 2-disk RAID1 pool is created, test data written, pool locked.
# A pending-op.json is injected to simulate an interrupted add of a third disk.
# Tests exercise: unlock blocked by journal, recover self-mounts and rebuilds,
# data intact, normal operations resume.

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
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid add {key}=/dev/disk/by-id/virtio-{key} --passphrase-stdin --yes"
    )


# --- Phase 1: Build 2-disk RAID1 pool and write test data ---

with subtest("Build 2-disk pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed("mountpoint -q /mnt/storage")

    # Write test data
    machine.succeed("echo 'recovery-test-data' > /mnt/storage/testfile.txt")
    machine.succeed("sync")

    # Capture pool.json for journal construction
    pool_json = json.loads(machine.succeed("cat /var/lib/braid/pool.json"))

# --- Phase 2: Lock pool and inject pending-op.json ---

with subtest("Lock pool and inject journal"):
    machine.succeed("braid lock")
    machine.fail("mountpoint -q /mnt/storage")

    # Build a pending-op.json simulating an interrupted add of disk3.
    # OpKind uses #[serde(tag = "op")] internally-tagged representation,
    # so the "op" field contains an object with the discriminant inside.
    journal = {
        "started_at": "2026-01-01T00:00:00Z",
        "op": {
            "op": "Add",
            "disks": {"disk3": "/dev/disk/by-id/virtio-disk3"},
        },
        "pre_membership": pool_json,
        "target_membership": pool_json,
    }
    journal_json = json.dumps(journal)
    machine.succeed(
        f"cat > /var/lib/braid/pending-op.json << 'JOURNAL_EOF'\n"
        f"{journal_json}\n"
        f"JOURNAL_EOF"
    )
    machine.succeed("test -f /var/lib/braid/pending-op.json")

# --- Phase 3: braid unlock must fail ---

with subtest("braid unlock refuses with journal present"):
    exit_code, output = machine.execute(
        f"printf '%s\\n' {pq} | braid unlock --passphrase-stdin 2>&1"
    )
    assert exit_code != 0, f"unlock should fail, but got exit {exit_code}"
    assert "interrupted operation" in output, (
        f"Expected 'interrupted operation' in output, got: {output}"
    )

# --- Phase 4: braid recover self-mounts and recovers ---

with subtest("braid recover self-mounts and rebuilds pool.json"):
    machine.succeed(
        f"printf '%s\\n' {pq} | braid recover --passphrase-stdin"
    )

    # Pool must be mounted
    machine.succeed("mountpoint -q /mnt/storage")

    # pool.json must exist and contain disk1 + disk2
    recovered = json.loads(machine.succeed("cat /var/lib/braid/pool.json"))
    assert "disk1" in recovered["disks"], f"disk1 missing from recovered pool.json: {recovered}"
    assert "disk2" in recovered["disks"], f"disk2 missing from recovered pool.json: {recovered}"

    # pending-op.json must be cleared
    machine.fail("test -f /var/lib/braid/pending-op.json")

# --- Phase 5: Test data intact ---

with subtest("Test data intact after recovery"):
    content = machine.succeed("cat /mnt/storage/testfile.txt").strip()
    assert content == "recovery-test-data", f"Expected 'recovery-test-data', got: {content}"

# --- Phase 6: Normal operations resume ---

with subtest("Normal operations resume after recovery"):
    machine.succeed("braid lock")
    machine.fail("mountpoint -q /mnt/storage")

    machine.succeed(
        f"printf '%s\\n' {pq} | braid unlock --passphrase-stdin"
    )
    machine.succeed("mountpoint -q /mnt/storage")

    # Data still there
    content = machine.succeed("cat /mnt/storage/testfile.txt").strip()
    assert content == "recovery-test-data", f"Expected 'recovery-test-data', got: {content}"

machine.shutdown()
