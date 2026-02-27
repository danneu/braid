# Test: replace a dead (missing) disk
#
# Intent:
# - What behavior this test (tries to) verify.
#   - `braid replace --old <dead> --new <new>` succeeds when the old disk has
#     been physically removed (LUKS mapper closed, device missing from pool).
#     Covers both auto-detect (single missing device → EvictionTarget::Missing)
#     and explicit `--missing-id <devid>` (→ EvictionTarget::Devid).
#
# Why it exists:
# - What risk/regression this protects against.
#   - Dead disk replacement is the original braid replace use case. Only unit
#     tests cover the resolution logic; this is the first end-to-end VM test
#     for the dead-disk path.
#
# Scenario:
# - Real-world situation this models.
#   - A drive fails in a 3-drive NAS. The operator plugs in a new drive and
#     runs `braid replace` to swap it in. Later a second drive dies and is
#     replaced using `--missing-id` to disambiguate.

import json
import re

start_all()
machine.wait_for_unit("multi-user.target")

import shlex

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def read_disk_map():
    raw = machine.succeed("cat /var/lib/braid/disk-map.json")
    return json.loads(raw)


def add_cmd(name):
    passphrase_q = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {passphrase_q} | "
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid add {name} --passphrase-stdin --yes"
    )


def replace_cmd(old, new, extra=""):
    passphrase_q = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {passphrase_q} | "
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid replace --old {old} --new {new} --passphrase-stdin --yes {extra}"
    )


def get_devid(mapper_name):
    """Extract the btrfs devid for a given mapper from `btrfs fi show`."""
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    for line in fi_show.splitlines():
        if mapper_name in line:
            m = re.search(r"devid\s+(\d+)", line)
            if m:
                return int(m.group(1))
    raise AssertionError(f"devid not found for {mapper_name} in:\n{fi_show}")


# --- Phase 0: Build 3-drive RAID1 pool ---

with subtest("Setup: build 3-drive pool with test data"):
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

# --- Phase 1: Kill disk2, replace with disk4 (auto-detect single missing) ---

with subtest("Kill disk2: simulate drive failure"):
    # Record disk3's devid while pool is healthy (needed for Phase 2)
    disk3_devid = get_devid("braid-disk3")
    print(f"disk3 devid = {disk3_devid}")

    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup close braid-disk2")
    machine.succeed("mount -o degraded /dev/mapper/braid-disk1 /mnt/storage")

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print(f"Pool after disk2 death:\n{fi_show}")
    assert "missing" in fi_show.lower(), f"Expected missing device:\n{fi_show}"

with subtest("Replace dead disk2 with disk4 (auto-detect)"):
    result = machine.succeed(replace_cmd("disk2", "disk4"))
    print(f"braid replace output:\n{result}")

with subtest("Pool healthy after dead replace: disk2 gone, disk4 present"):
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print(f"Pool after replace disk2→disk4:\n{fi_show}")

    assert "/dev/mapper/braid-disk4" in fi_show, (
        f"New disk braid-disk4 missing from pool:\n{fi_show}"
    )
    assert "braid-disk2" not in fi_show, (
        f"Old disk braid-disk2 should be removed:\n{fi_show}"
    )
    assert "missing" not in fi_show.lower(), (
        f"Pool should have no missing devices:\n{fi_show}"
    )

    devid_count = fi_show.count("devid")
    assert devid_count == 3, f"Expected 3 devices, got {devid_count}:\n{fi_show}"

    df_output = machine.succeed("btrfs fi df /mnt/storage")
    assert "RAID1" in df_output, f"Expected RAID1 profile:\n{df_output}"

with subtest("Data intact after dead replace (auto-detect)"):
    content = machine.succeed("cat /mnt/storage/precious.txt").strip()
    assert content == "important data", f"Expected 'important data', got '{content}'"

with subtest("Disk map updated after dead replace (auto-detect)"):
    dm = read_disk_map()
    assert "disk2" not in dm["disks"], f"disk2 still in map: {dm}"
    assert "disk4" in dm["disks"], f"disk4 missing from map: {dm}"
    for name in ["disk1", "disk3"]:
        assert name in dm["disks"], f"{name} missing from map: {dm}"

# --- Phase 2: Kill disk3, replace with disk5 (explicit --missing-id) ---

with subtest("Kill disk3: simulate second drive failure"):
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup close braid-disk3")
    machine.succeed("mount -o degraded /dev/mapper/braid-disk1 /mnt/storage")

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print(f"Pool after disk3 death:\n{fi_show}")
    assert "missing" in fi_show.lower(), f"Expected missing device:\n{fi_show}"

with subtest("Wrong --missing-id is rejected"):
    # Use disk5 for the wrong-ID test. Note: braid will partially complete
    # (LUKS format + add + balance) before failing at the remove step with
    # the bogus devid. This pollutes pool state, so the correct test below
    # uses a separate disk (disk6).
    wrong_devid = 9999
    (status, output) = machine.execute(
        replace_cmd("disk3", "disk5", extra=f"--missing-id {wrong_devid}") + " 2>&1"
    )
    assert status != 0, (
        f"Expected failure with wrong --missing-id {wrong_devid}, got exit 0: {output}"
    )
    print(f"Wrong --missing-id error (expected):\n{output}")

    # Clean up: remove disk5 from pool (it was added before the remove failed)
    # and close its LUKS mapper so the pool is back to: disk1 + disk4 + missing(disk3)
    machine.succeed("btrfs device remove /dev/mapper/braid-disk5 /mnt/storage")
    machine.succeed("cryptsetup close braid-disk5")

with subtest("Replace dead disk3 with disk6 (correct --missing-id)"):
    result = machine.succeed(
        replace_cmd("disk3", "disk6", extra=f"--missing-id {disk3_devid}")
    )
    print(f"braid replace output:\n{result}")

with subtest("Pool healthy after dead replace: disk3 gone, disk6 present"):
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print(f"Pool after replace disk3→disk6:\n{fi_show}")

    assert "/dev/mapper/braid-disk6" in fi_show, (
        f"New disk braid-disk6 missing from pool:\n{fi_show}"
    )
    assert "braid-disk3" not in fi_show, (
        f"Old disk braid-disk3 should be removed:\n{fi_show}"
    )
    assert "missing" not in fi_show.lower(), (
        f"Pool should have no missing devices:\n{fi_show}"
    )

    devid_count = fi_show.count("devid")
    assert devid_count == 3, f"Expected 3 devices, got {devid_count}:\n{fi_show}"

    df_output = machine.succeed("btrfs fi df /mnt/storage")
    assert "RAID1" in df_output, f"Expected RAID1 profile:\n{df_output}"

with subtest("Data intact after dead replace (--missing-id)"):
    content = machine.succeed("cat /mnt/storage/precious.txt").strip()
    assert content == "important data", f"Expected 'important data', got '{content}'"

with subtest("Disk map updated after dead replace (--missing-id)"):
    dm = read_disk_map()
    assert "disk3" not in dm["disks"], f"disk3 still in map: {dm}"
    assert "disk6" in dm["disks"], f"disk6 missing from map: {dm}"
    for name in ["disk1", "disk4"]:
        assert name in dm["disks"], f"{name} missing from map: {dm}"

machine.shutdown()
