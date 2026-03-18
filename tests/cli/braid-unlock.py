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

# --- Test 2b: Bootstrap disk-map on fresh system ---

with subtest("Test 2b: bootstrap disk-map on fresh system"):
    close_all()

    # Save expected entries (written by `braid add`)
    disk_map_raw = machine.succeed("cat /var/lib/braid/disk-map.json")
    expected = json.loads(disk_map_raw)

    # Simulate fresh machine: delete disk-map
    machine.succeed("rm /var/lib/braid/disk-map.json")

    # Unlock should succeed and bootstrap disk-map
    machine.succeed(unlock_cmd(passphrase))
    machine.succeed("mountpoint -q /mnt/storage")

    # Verify disk-map was recreated with all 3 disks
    new_raw = machine.succeed("cat /var/lib/braid/disk-map.json")
    new_map = json.loads(new_raw)

    assert set(new_map["disks"].keys()) == {"disk1", "disk2", "disk3"}, \
        f"Expected 3 disks in bootstrapped map, got: {set(new_map['disks'].keys())}"

    # Identity fields must match originals (added_at will differ)
    for name in ["disk1", "disk2", "disk3"]:
        for field in ["by_id", "luks_uuid", "devid"]:
            assert new_map["disks"][name][field] == expected["disks"][name][field], \
                f"{name}.{field}: expected {expected['disks'][name][field]}, " \
                f"got {new_map['disks'][name][field]}"

# --- Test 2c: Swapped config refuses to bootstrap wrong entries ---

with subtest("Test 2c: swapped config refuses to bootstrap wrong entries"):
    close_all()

    # Save original disk-map for restoration
    original_map = machine.succeed("cat /var/lib/braid/disk-map.json")

    # Simulate fresh machine: delete disk-map
    machine.succeed("rm /var/lib/braid/disk-map.json")

    # Create config with disk1/disk2 by-id paths swapped
    swapped_config = json.dumps({
        "disks": {
            "disk1": {"by_id": "/dev/disk/by-id/virtio-disk2"},
            "disk2": {"by_id": "/dev/disk/by-id/virtio-disk1"},
            "disk3": {"by_id": "/dev/disk/by-id/virtio-disk3"},
        },
        "mount_point": "/mnt/storage",
    })
    machine.succeed(f"echo '{swapped_config}' > /tmp/swapped.json")

    # Unlock with swapped config — mounts fine (btrfs doesn't care about names)
    machine.succeed(unlock_cmd(passphrase, extra="--config /tmp/swapped.json"))
    machine.succeed("mountpoint -q /mnt/storage")

    # Bootstrap should record ONLY disk3 (label matches).
    # disk1 and disk2 are swapped: LUKS labels don't match config names.
    new_raw = machine.succeed("cat /var/lib/braid/disk-map.json")
    new_map = json.loads(new_raw)
    assert set(new_map["disks"].keys()) == {"disk3"}, \
        f"Expected only disk3 (label-verified), got: {set(new_map['disks'].keys())}"

    # Restore for subsequent tests: re-unlock with correct config so pool is mounted
    close_all()
    machine.succeed(f"echo '{original_map}' > /var/lib/braid/disk-map.json")
    machine.succeed(unlock_cmd(passphrase))

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

# --- Test 4a: Missing disk — refuses degraded by default ---

with subtest("Test 4a: missing disk — refuses degraded by default"):
    close_all()

    # Remove disk3's by-id symlink to simulate unplugged disk
    machine.succeed("rm -f /dev/disk/by-id/virtio-disk3")

    ret = machine.execute(unlock_cmd(passphrase) + " 2>&1")
    assert ret[0] != 0, "Expected non-zero exit for degraded refusal"
    assert "refusing to mount degraded" in ret[1], \
        f"Expected 'refusing to mount degraded' in output, got: {ret[1]}"
    assert "--allow-degraded" in ret[1], \
        f"Expected '--allow-degraded' hint in output, got: {ret[1]}"
    machine.fail("mountpoint -q /mnt/storage")

# --- Test 4b: Missing disk — --allow-degraded mounts degraded ---

with subtest("Test 4b: missing disk — --allow-degraded mounts degraded"):
    machine.succeed(unlock_cmd(passphrase, extra="--allow-degraded"))

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

# --- Test 5a: Identity mismatch blocks degraded unlock ---

with subtest("Test 5a: identity mismatch blocks degraded unlock"):
    close_all()

    # Save current disk-map
    disk_map_raw = machine.succeed("cat /var/lib/braid/disk-map.json")
    disk_map = json.loads(disk_map_raw)

    # Tamper: change disk3's by_id to a wrong value
    tampered = dict(disk_map)
    tampered["disks"] = dict(tampered["disks"])
    tampered["disks"]["disk3"] = dict(tampered["disks"]["disk3"])
    tampered["disks"]["disk3"]["by_id"] = "/dev/disk/by-id/virtio-WRONG"
    machine.succeed(f"echo '{json.dumps(tampered)}' > /var/lib/braid/disk-map.json")

    # Remove disk3 symlink to simulate unplugged disk
    machine.succeed("rm -f /dev/disk/by-id/virtio-disk3")

    cmd = unlock_cmd(passphrase, extra="--allow-degraded") + " 2>&1"
    ret = machine.execute(cmd)
    assert ret[0] != 0, "Expected non-zero exit for identity mismatch"
    assert "not allowed" in ret[1], \
        f"Expected 'not allowed' in output, got: {ret[1]}"

    # Restore disk-map and symlink
    machine.succeed(f"echo '{json.dumps(disk_map)}' > /var/lib/braid/disk-map.json")
    machine.succeed("udevadm trigger && udevadm settle")
    machine.succeed("test -e /dev/disk/by-id/virtio-disk3")

# --- Test 5b: Identity mismatch blocks healthy unlock ---

with subtest("Test 5b: identity mismatch blocks healthy unlock"):
    close_all()

    # Save current disk-map
    disk_map_raw = machine.succeed("cat /var/lib/braid/disk-map.json")
    disk_map = json.loads(disk_map_raw)

    # Tamper: change disk1's by_id to a wrong value (all disks present)
    tampered = dict(disk_map)
    tampered["disks"] = dict(tampered["disks"])
    tampered["disks"]["disk1"] = dict(tampered["disks"]["disk1"])
    tampered["disks"]["disk1"]["by_id"] = "/dev/disk/by-id/virtio-WRONG"
    machine.succeed(f"echo '{json.dumps(tampered)}' > /var/lib/braid/disk-map.json")

    cmd = unlock_cmd(passphrase) + " 2>&1"
    ret = machine.execute(cmd)
    assert ret[0] != 0, "Expected non-zero exit for identity mismatch"
    assert "not allowed" in ret[1], \
        f"Expected 'not allowed' in output, got: {ret[1]}"

    # Restore disk-map
    machine.succeed(f"echo '{json.dumps(disk_map)}' > /var/lib/braid/disk-map.json")

# --- Test 6: Wrong passphrase ---

with subtest("Test 6: wrong passphrase rejected"):
    close_all()

    ret = machine.execute(unlock_cmd("wrongpassphrase"))
    assert ret[0] != 0, "Expected non-zero exit for wrong passphrase"

    # No mappers should have been opened
    machine.fail("test -e /dev/mapper/braid-disk1")

# --- Test 7: Uninitialized disk ---

with subtest("Test 7: uninitialized disk detected"):
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
