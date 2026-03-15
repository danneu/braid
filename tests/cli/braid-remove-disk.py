# Test: braid remove / braid remove-missing lifecycle
#
# What: Tests the two-command split for disk removal:
#   - `braid remove <key>` only removes present disks from the pool
#   - `braid remove-missing` explicitly removes missing/dead devices
#
# Why: `braid remove` previously had a dangerous implicit fallback — if a disk
# wasn't found in the pool but btrfs reported missing devices, it silently
# performed `btrfs device remove missing`. A typo or stale key could cause a
# destructive action on an unrelated device. The fix: `braid remove` only
# removes present disks; removing missing devices is a separate explicit
# command `braid remove-missing`.
#
# Dependencies: braid add (builds the test pool).

import json

start_all()
machine.wait_for_unit("multi-user.target")

import shlex

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def read_disk_map():
    """Read and parse the disk map file."""
    raw = machine.succeed("cat /var/lib/braid/disk-map.json")
    return json.loads(raw)


def add_cmd(key):
    """Build a `braid add <key> --yes` command with env vars."""
    passphrase_q = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {passphrase_q} | "
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid add {key} --passphrase-stdin --yes"
    )


def remove_cmd(key, extra=""):
    """Build a `braid remove <key> --yes` command."""
    return f"braid remove {key} --yes {extra}"


def remove_missing_cmd(extra=""):
    """Build a `braid remove-missing --yes` command."""
    return f"braid remove-missing --yes {extra}"


# --- Phase 0: Build 3-drive RAID1 pool ---

with subtest("Setup: build 3-drive pool with braid add"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed(add_cmd("disk3"))

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    for name in ["braid-disk1", "braid-disk2", "braid-disk3"]:
        assert f"/dev/mapper/{name}" in fi_show, f"{name} missing:\n{fi_show}"

    df_output = machine.succeed("btrfs fi df /mnt/storage")
    assert "RAID1" in df_output, f"Expected RAID1 profile:\n{df_output}"

    machine.succeed("echo 'important data' > /mnt/storage/precious.txt")
    machine.succeed("sync")

with subtest("Disk map has all 3 disks after add"):
    dm = read_disk_map()
    for name in ["disk1", "disk2", "disk3"]:
        assert name in dm["disks"], f"{name} missing from disk map: {dm}"
        entry = dm["disks"][name]
        assert "by_id" in entry, f"Missing by_id for {name}"
        assert "luks_uuid" in entry, f"Missing luks_uuid for {name}"
        assert "devid" in entry, f"Missing devid for {name}"
        assert "added_at" in entry, f"Missing added_at for {name}"

# --- Phase 1: Validation errors ---

with subtest("Remove nonexistent disk fails with 'not found in pool'"):
    # 'nonexistent' is not in the pool and no missing devices exist,
    # so braid remove should fail with a clear error.
    (status, output) = machine.execute(remove_cmd("nonexistent") + " 2>&1")
    assert status != 0, f"Expected failure, got exit 0: {output}"
    assert "not found in pool" in output, f"Expected 'not found in pool' in error:\n{output}"

# --- Phase 1b: remove-missing with no missing devices ---

with subtest("remove-missing fails when no devices are missing"):
    (status, output) = machine.execute(remove_missing_cmd() + " 2>&1")
    assert status != 0, f"Expected failure, got exit 0: {output}"
    assert "no missing" in output.lower(), f"Expected 'no missing' in error:\n{output}"

# --- Phase 2: Graceful remove (disk3 present in pool) ---

with subtest("Graceful remove of disk3"):
    machine.succeed(remove_cmd("disk3"))

with subtest("disk3 gone from pool, disk1+disk2 remain, RAID1 profile"):
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print(f"Pool after graceful remove:\n{fi_show}")
    assert "braid-disk3" not in fi_show, f"disk3 still in pool:\n{fi_show}"
    for name in ["braid-disk1", "braid-disk2"]:
        assert f"/dev/mapper/{name}" in fi_show, f"{name} missing:\n{fi_show}"

    df_output = machine.succeed("btrfs fi df /mnt/storage")
    assert "RAID1" in df_output, f"Expected RAID1 profile:\n{df_output}"

with subtest("Disk map updated after graceful remove"):
    dm = read_disk_map()
    assert "disk3" not in dm["disks"], f"disk3 still in map after remove: {dm}"
    assert "disk1" in dm["disks"], f"disk1 missing from map: {dm}"
    assert "disk2" in dm["disks"], f"disk2 missing from map: {dm}"

with subtest("LUKS mapper closed after graceful remove"):
    machine.fail("test -e /dev/mapper/braid-disk3")

with subtest("Data intact after graceful remove"):
    content = machine.succeed("cat /mnt/storage/precious.txt").strip()
    assert content == "important data", f"Expected 'important data', got '{content}'"

# --- Phase 3: Redundancy warning ---
# Pool has disk1 + disk2. Removing disk2 leaves 1 disk (no redundancy).
# With --yes, the interactive redundancy confirmation is bypassed.

with subtest("Redundancy-reducing remove with --yes succeeds"):
    machine.succeed(remove_cmd("disk2"))

with subtest("Pool has 1 device with single profile after redundancy removal"):
    # btrfs RAID1 requires ≥2 devices, so removing the second-to-last device
    # must first balance to single profile before the device remove can succeed.
    # Verify both the device count and that the profile converted to single.
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print(f"Pool after redundancy removal:\n{fi_show}")
    devid_count = fi_show.count("devid")
    assert devid_count == 1, f"Expected 1 device, got {devid_count}:\n{fi_show}"
    assert "braid-disk1" in fi_show, f"disk1 missing:\n{fi_show}"

    df_output = machine.succeed("btrfs fi df /mnt/storage")
    assert "single" in df_output.lower(), f"Expected single profile after 2→1 remove:\n{df_output}"
    assert "raid1" not in df_output.lower(), f"RAID1 profile should not remain after 2→1 remove:\n{df_output}"

with subtest("LUKS mapper closed after redundancy removal"):
    machine.fail("test -e /dev/mapper/braid-disk2")

with subtest("Data intact after redundancy removal"):
    content = machine.succeed("cat /mnt/storage/precious.txt").strip()
    assert content == "important data", f"Expected 'important data', got '{content}'"

# --- Phase 4: Remove of a dead disk must fail ---

with subtest("Rebuild pool: re-add disk2 and disk3"):
    # After braid remove, disks still have LUKS headers with braid labels but
    # no btrfs superblock. braid add refuses this ambiguous state — wipe the
    # LUKS headers so they go through the fresh-disk path.
    machine.succeed("dd if=/dev/zero of=/dev/disk/by-id/virtio-disk2 bs=1M count=4")
    machine.succeed("dd if=/dev/zero of=/dev/disk/by-id/virtio-disk3 bs=1M count=4")
    machine.succeed(add_cmd("disk2"))
    machine.succeed(add_cmd("disk3"))

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    for name in ["braid-disk1", "braid-disk2", "braid-disk3"]:
        assert f"/dev/mapper/{name}" in fi_show, f"{name} missing after rebuild:\n{fi_show}"

    df_output = machine.succeed("btrfs fi df /mnt/storage")
    assert "RAID1" in df_output, f"Expected RAID1 after rebuild:\n{df_output}"

with subtest("Simulate disk3 death and mount degraded"):
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup close braid-disk3")
    machine.succeed("mount -o degraded /dev/mapper/braid-disk1 /mnt/storage")

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print(f"Pool after simulated death:\n{fi_show}")
    assert "missing" in fi_show.lower(), f"Expected missing device:\n{fi_show}"

with subtest("braid remove disk3 fails for dead disk"):
    (status, output) = machine.execute(remove_cmd("disk3") + " 2>&1")
    assert status != 0, f"Expected failure, got exit 0: {output}"
    assert "not found in pool" in output, f"Expected 'not found in pool' in error:\n{output}"
    assert "missing" in output.lower(), f"Expected mention of missing devices:\n{output}"
    assert "braid replace" in output, f"Expected suggestion to use braid replace:\n{output}"
    assert "remove-missing" in output, f"Expected suggestion to use remove-missing:\n{output}"

with subtest("Pool unchanged after failed remove"):
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "missing" in fi_show.lower(), f"Missing device should still be present:\n{fi_show}"

with subtest("Disk map unchanged after failed remove"):
    dm = read_disk_map()
    # Map should still have all 3 disks from the rebuild
    assert "disk1" in dm["disks"], f"disk1 missing from map: {dm}"
    assert "disk2" in dm["disks"], f"disk2 missing from map: {dm}"
    assert "disk3" in dm["disks"], f"disk3 missing from map: {dm}"

# --- Phase 5: Explicit remove-missing succeeds ---

with subtest("remove-missing succeeds for dead disk"):
    machine.succeed(remove_missing_cmd())

with subtest("No missing devices after remove-missing"):
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print(f"Pool after remove-missing:\n{fi_show}")
    assert "missing" not in fi_show.lower(), f"Still has missing device:\n{fi_show}"

with subtest("Disk map pruned after remove-missing"):
    dm = read_disk_map()
    assert "disk3" not in dm["disks"], f"disk3 still in map after remove-missing: {dm}"
    assert "disk1" in dm["disks"], f"disk1 missing from map: {dm}"
    assert "disk2" in dm["disks"], f"disk2 missing from map: {dm}"

with subtest("Data intact after remove-missing"):
    content = machine.succeed("cat /mnt/storage/precious.txt").strip()
    assert content == "important data", f"Expected 'important data', got '{content}'"

# Phase 5b (multi-missing disambiguation) is not tested here because btrfs
# RAID1 refuses writable mount with 2 of 3 devices missing ("max tolerance
# is 1 for writable mount"). The disambiguation logic (multiple missing →
# require --missing-id) is validated by the error path in remove_missing.rs.
# A dedicated test with 4+ disks could cover this if needed.

machine.shutdown()
