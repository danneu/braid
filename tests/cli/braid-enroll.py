# Test: braid-enroll
#
# Intent: Verify that `braid enroll` enrolls a binary keyfile into
# LUKS slot 1 on all pool disks, and that `braid unlock --key-file` can
# subsequently open them.
#
# Why it exists: The keyfile enrollment path uses different cryptsetup
# semantics than passphrase (raw bytes, explicit slot, no PBKDF). If
# enrollment silently fails or targets the wrong slot, auto-unlock breaks
# at 3 AM when nobody is watching.
#
# Scenario: 2-disk RAID1 pool. Generate 4096-byte random keyfile. Enroll
# into both disks. Lock pool. Unlock with keyfile. Verify data intact.
# Re-enroll (idempotent). Verify passphrase path still works.

import shlex

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def add_cmd(key):
    pq = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {pq} | "
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid add {key}=/dev/disk/by-id/virtio-{key} --passphrase-stdin --yes"
    )


def close_all():
    machine.execute("umount /mnt/storage 2>/dev/null || true")
    for k in ["disk1", "disk2"]:
        machine.execute(f"cryptsetup close braid-{k} 2>/dev/null || true")


# --- Setup: Create 2-disk RAID1 pool ---

with subtest("Setup: create 2-disk pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed("echo 'keyfile test data' > /mnt/storage/keytest.txt")
    machine.succeed("sync")

with subtest("Generate random keyfile"):
    machine.succeed("dd if=/dev/urandom of=/tmp/braid.key bs=4096 count=1 iflag=fullblock")
    machine.succeed("chmod 400 /tmp/braid.key")

# --- Test 1: Enroll keyfile into both disks ---

with subtest("Test 1: enroll keyfile into all pool disks"):
    pq = shlex.quote(passphrase)
    machine.succeed(
        f"printf '%s\\n' {pq} | braid enroll /tmp --passphrase-stdin"
    )

    # Verify slot 1 is occupied on both disks
    for dev in ["virtio-disk1", "virtio-disk2"]:
        dump = machine.succeed(f"cryptsetup luksDump --dump-json-metadata /dev/disk/by-id/{dev}")
        assert '"1"' in dump, f"slot 1 not found in luksDump for {dev}: {dump}"

# --- Test 2: Lock, then unlock with keyfile ---

with subtest("Test 2: unlock with keyfile"):
    close_all()

    machine.fail("mountpoint -q /mnt/storage")

    machine.succeed("braid unlock --key-file /tmp/braid.key")

    machine.succeed("mountpoint -q /mnt/storage")
    for k in ["disk1", "disk2"]:
        machine.succeed(f"test -e /dev/mapper/braid-{k}")

    content = machine.succeed("cat /mnt/storage/keytest.txt").strip()
    assert content == "keyfile test data", f"Expected 'keyfile test data', got '{content}'"

# --- Test 3: Re-enroll is idempotent ---

with subtest("Test 3: re-enroll is idempotent"):
    pq = shlex.quote(passphrase)
    machine.succeed(
        f"printf '%s\\n' {pq} | braid enroll /tmp --passphrase-stdin"
    )

# --- Test 4: Passphrase still works after keyfile enrollment ---

with subtest("Test 4: passphrase still works"):
    close_all()

    pq = shlex.quote(passphrase)
    machine.succeed(f"printf '%s\\n' {pq} | braid unlock --passphrase-stdin")

    machine.succeed("mountpoint -q /mnt/storage")
    content = machine.succeed("cat /mnt/storage/keytest.txt").strip()
    assert content == "keyfile test data", f"Expected 'keyfile test data', got '{content}'"

# --- Test 5: Preflight detects slot conflict before any enrollment ---

with subtest("Test 5: preflight detects slot-1 conflict before any mutation"):
    # This is the regression test for the two-phase refactor. The old code
    # enrolled disk1 (slot 1 empty) before discovering that disk2 had a
    # slot-1 conflict, leaving the pool partially mutated. The new code
    # detects the conflict during planning and fails before any mutation.
    close_all()

    # Remove the keyfile from both disks so slot 1 is empty again
    for dev in ["virtio-disk1", "virtio-disk2"]:
        machine.succeed(f"cryptsetup luksKillSlot --batch-mode /dev/disk/by-id/{dev} 1")

    # Put an unknown key into disk2's slot 1 (simulates an external tool
    # or manual cryptsetup having claimed the slot)
    machine.succeed("dd if=/dev/urandom of=/tmp/conflict.key bs=32 count=1 iflag=fullblock")
    pq = shlex.quote(passphrase)
    machine.succeed(
        f"printf '%s\\n' {pq} | "
        f"cryptsetup luksAddKey --key-slot 1 /dev/disk/by-id/virtio-disk2 /tmp/conflict.key"
    )

    # Verify disk1 slot 1 is empty (the keyfile does not work on disk1)
    machine.fail(
        "cryptsetup open --test-passphrase --key-file /tmp/braid.key /dev/disk/by-id/virtio-disk1"
    )

    # Try to enroll — should fail due to slot-1 conflict on disk2
    machine.fail(
        f"printf '%s\\n' {pq} | braid enroll /tmp --passphrase-stdin"
    )

    # Verify disk1's slot 1 is STILL empty — preflight prevented mutation.
    # If the old (non-preflight) code were running, disk1 would have been
    # enrolled before the disk2 conflict was detected.
    machine.fail(
        "cryptsetup open --test-passphrase --key-file /tmp/braid.key /dev/disk/by-id/virtio-disk1"
    )

machine.shutdown()
