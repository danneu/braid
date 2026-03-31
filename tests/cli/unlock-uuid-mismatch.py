# Test: unlock LUKS UUID mismatch
#
# Intent: Verify `braid unlock` fatally errors when a disk's LUKS UUID doesn't
# match the UUID stored in pool.json.
#
# Why it exists: A swapped, reformatted, or corrupted drive could silently mount
# wrong data if the UUID check fails. This is the highest-blast-radius failure
# mode — the user gets no error, but their pool contains data from a different
# drive. The check (cli/src/mount.rs:81-91) is unit-tested, but this test
# exercises the real cryptsetup luksUUID probe → comparison → fatal error
# pipeline end-to-end.
#
# Scenario: User has a 2-disk RAID1 pool. A drive fails and is replaced with a
# new one that happens to be LUKS-formatted with the same passphrase (e.g. from
# another machine). On the next unlock, braid detects the UUID mismatch and
# refuses to proceed, preventing silent data corruption.

import json
import shlex

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def add_cmd(key):
    """Build a `braid add <key> --yes` command."""
    pq = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {pq} | "
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid add {key}=/dev/disk/by-id/virtio-{key} --passphrase-stdin --yes"
    )


def unlock_cmd():
    """Build a `braid unlock` command."""
    pq = shlex.quote(passphrase)
    return f"printf '%s\\n' {pq} | braid unlock --passphrase-stdin"


def close_all():
    """Unmount pool and close all LUKS mappers."""
    machine.execute("umount /mnt/storage 2>/dev/null || true")
    for k in ["disk1", "disk2"]:
        machine.execute(f"cryptsetup close braid-{k} 2>/dev/null || true")


# --- Setup: Create a 2-disk RAID1 pool and enrich pool.json ---

with subtest("Setup: create pool and enrich pool.json with LUKS UUIDs"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))

    # Tear down and re-unlock so pool.json gets enriched with luks_uuid fields
    close_all()
    machine.succeed(unlock_cmd())
    machine.succeed("mountpoint -q /mnt/storage")

    # Verify enrichment happened
    pool_raw = machine.succeed("cat /var/lib/braid/pool.json")
    pool = json.loads(pool_raw)
    expected_uuid = pool["disks"]["disk2"]["luks_uuid"]
    assert expected_uuid is not None, (
        "disk2.luks_uuid should be populated after unlock"
    )

# --- Test: LUKS UUID mismatch detected on reformatted disk ---

with subtest("UUID mismatch: reformatted disk2 detected and rejected"):
    close_all()

    # Reformat disk2 with a fresh LUKS container (new UUID, same passphrase).
    # This simulates a swapped or reformatted drive.
    pq = shlex.quote(passphrase)
    machine.succeed(
        f"echo -n {pq} | cryptsetup luksFormat --batch-mode --key-file=- "
        f"{luks_opts} /dev/disk/by-id/virtio-disk2"
    )

    # Read the new UUID from the reformatted disk
    new_uuid = machine.succeed(
        "cryptsetup luksUUID /dev/disk/by-id/virtio-disk2"
    ).strip()
    assert new_uuid != expected_uuid, (
        f"New UUID should differ from original: {new_uuid} == {expected_uuid}"
    )

    # Attempt unlock — must fail with UUID mismatch
    ret = machine.execute(unlock_cmd() + " 2>&1")
    assert ret[0] != 0, "Expected non-zero exit for UUID mismatch"
    assert "LUKS UUID mismatch" in ret[1], (
        f"Expected 'LUKS UUID mismatch' in output, got: {ret[1]}"
    )
    assert "disk2" in ret[1], (
        f"Expected 'disk2' named in output, got: {ret[1]}"
    )
    assert expected_uuid in ret[1], (
        f"Expected original UUID {expected_uuid} in output, got: {ret[1]}"
    )
    assert new_uuid in ret[1], (
        f"Expected new UUID {new_uuid} in output, got: {ret[1]}"
    )

    # Pool must NOT be mounted
    machine.fail("mountpoint -q /mnt/storage")

    # No LUKS mappers should have been opened — the error fires during
    # probing (before any cryptsetup open calls)
    for k in ["disk1", "disk2"]:
        machine.fail(f"test -e /dev/mapper/braid-{k}")

machine.shutdown()
