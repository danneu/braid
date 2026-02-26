# Test: braid unlock
#
# Intent: Verify `braid unlock` opens LUKS volumes and mounts the btrfs pool
# in one idempotent command.
#
# Why it exists: After a NixOS rebuild or missed initrd unlock window, there is
# no CLI path to open LUKS volumes and mount the pool. Users must manually run
# cryptsetup open + btrfs device scan + mount. This test ensures `braid unlock`
# handles all the common scenarios correctly.
#
# Scenario: 3-disk RAID1 pool is set up via `braid add`, then everything is
# torn down (unmount + cryptsetup close). Tests exercise: happy path, idempotent
# re-run, partial state, missing disk (degraded), wrong passphrase, and
# uninitialized disk.

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
        f"braid add {key} --passphrase-stdin --yes"
    )


def unlock_cmd(passphrase_str=None, extra=""):
    """Build a `braid unlock` command."""
    if passphrase_str is not None:
        pq = shlex.quote(passphrase_str)
        return f"printf '%s\\n' {pq} | braid unlock --passphrase-stdin {extra}"
    return f"braid unlock {extra}"


def close_all():
    """Unmount pool and close all LUKS mappers."""
    machine.execute("umount /mnt/storage 2>/dev/null || true")
    for k in ["disk1", "disk2", "disk3"]:
        machine.execute(f"cryptsetup close braid-{k} 2>/dev/null || true")


# --- Setup: Create a 3-disk RAID1 pool ---

with subtest("Setup: create 3-disk pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed(add_cmd("disk3"))

    # Write test data
    machine.succeed("echo 'persistent data' > /mnt/storage/test.txt")
    machine.succeed("sync")

    # Tear everything down
    close_all()

    # Verify pool is gone
    machine.fail("mountpoint -q /mnt/storage")
    machine.fail("test -e /dev/mapper/braid-disk1")

# --- Test 1: Happy path ---

with subtest("Test 1: happy path — all locked, unlock opens everything"):
    machine.succeed(unlock_cmd(passphrase))

    # Pool mounted
    machine.succeed("mountpoint -q /mnt/storage")

    # All mappers open
    for k in ["disk1", "disk2", "disk3"]:
        machine.succeed(f"test -e /dev/mapper/braid-{k}")

    # Data intact
    content = machine.succeed("cat /mnt/storage/test.txt").strip()
    assert content == "persistent data", f"Expected 'persistent data', got '{content}'"

# --- Test 2: Idempotent ---

with subtest("Test 2: idempotent — unlock again is a no-op"):
    machine.succeed(unlock_cmd(passphrase))

    # Still mounted, still works
    machine.succeed("mountpoint -q /mnt/storage")
    content = machine.succeed("cat /mnt/storage/test.txt").strip()
    assert content == "persistent data", f"Expected 'persistent data', got '{content}'"

# --- Test 3: Partial state ---

with subtest("Test 3: partial state — one mapper closed, pool unmounted"):
    # Close just disk1 and unmount
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup close braid-disk1")

    # disk2 and disk3 still open
    machine.succeed("test -e /dev/mapper/braid-disk2")
    machine.succeed("test -e /dev/mapper/braid-disk3")

    # Unlock should reopen disk1 and remount
    machine.succeed(unlock_cmd(passphrase))

    machine.succeed("mountpoint -q /mnt/storage")
    machine.succeed("test -e /dev/mapper/braid-disk1")
    content = machine.succeed("cat /mnt/storage/test.txt").strip()
    assert content == "persistent data", f"Expected 'persistent data', got '{content}'"

# --- Test 4: Missing disk (degraded mount) ---

with subtest("Test 4: missing disk — degraded mount"):
    close_all()

    # Remove disk3's by-id symlink to simulate unplugged disk
    machine.succeed("rm -f /dev/disk/by-id/virtio-disk3")

    machine.succeed(unlock_cmd(passphrase))

    machine.succeed("mountpoint -q /mnt/storage")

    # disk1 and disk2 open, disk3 absent
    machine.succeed("test -e /dev/mapper/braid-disk1")
    machine.succeed("test -e /dev/mapper/braid-disk2")

    # Data intact (RAID1 redundancy)
    content = machine.succeed("cat /mnt/storage/test.txt").strip()
    assert content == "persistent data", f"Expected 'persistent data', got '{content}'"

    # Restore symlink for subsequent tests
    close_all()
    # The virtio symlinks are managed by udev; trigger a rescan
    machine.succeed("udevadm trigger && udevadm settle")
    machine.succeed("test -e /dev/disk/by-id/virtio-disk3")

# --- Test 5: Wrong passphrase ---

with subtest("Test 5: wrong passphrase rejected"):
    close_all()

    ret = machine.execute(unlock_cmd("wrongpassphrase"))
    assert ret[0] != 0, "Expected non-zero exit for wrong passphrase"

    # No mappers should have been opened
    machine.fail("test -e /dev/mapper/braid-disk1")

# --- Test 6: Uninitialized disk ---

with subtest("Test 6: uninitialized disk detected"):
    close_all()

    # Write a temp config pointing at disk4 (raw, never braid add'd)
    raw_config = json.dumps({
        "disks": {
            "raw": {"by_id": "/dev/disk/by-id/virtio-disk4"},
        },
        "mount_point": "/mnt/storage",
    })
    machine.succeed(f"echo '{raw_config}' > /tmp/raw.json")

    # Redirect stderr to stdout so we can capture the error message
    cmd = unlock_cmd(passphrase, extra="--config /tmp/raw.json") + " 2>&1"
    ret = machine.execute(cmd)
    assert ret[0] != 0, "Expected non-zero exit for uninitialized disk"
    assert "not initialized" in ret[1], \
        f"Expected 'not initialized' in output, got: {ret}"

machine.shutdown()
