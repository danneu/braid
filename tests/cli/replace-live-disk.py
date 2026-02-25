# Test: replace a live disk in a healthy pool
#
# Intent:
# - What behavior this test (tries to) verify.
#   - `braid replace --old <live> --new <new>` succeeds for a live disk in a
#     healthy pool: the new disk is added first (RAID1 balance), then the old
#     disk is evicted (device remove + LUKS close). Data survives, profiles
#     remain correct, and the old mapper is fully released.
#
# Why it exists:
# - What risk/regression this protects against.
#   - Before this feature, `braid replace` only accepted dead/missing disks.
#     This test ensures the new live-replace path works end-to-end and that
#     the refactored shared eviction helper is wired correctly.
#
# Scenario:
# - Real-world situation this models (user/system story). Especially the
#   specific scenario that inspired this test (like a real world bug).
#   - Operator swaps a slow-but-alive drive for a faster one without
#     downtime. The pool stays healthy throughout the operation.

import json

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def read_disk_map():
    raw = machine.succeed("cat /var/lib/braid/disk-map.json")
    return json.loads(raw)


def add_cmd(name):
    return (
        f"BRAID_PASSPHRASE='{passphrase}' "
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid add {name} --yes"
    )


def replace_cmd(old, new, extra=""):
    return (
        f"BRAID_PASSPHRASE='{passphrase}' "
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid replace --old {old} --new {new} --yes {extra}"
    )


# --- Phase 0: Build 3-drive RAID1 pool ---

with subtest("Setup: build 3-drive pool"):
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

# --- Phase 1: Live replace disk2 → disk4 ---

with subtest("Live replace disk2 with disk4"):
    result = machine.succeed(replace_cmd("disk2", "disk4"))
    print(f"braid replace output:\n{result}")

with subtest("Pool healthy after live replace: disk2 removed, disk4 present"):
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print(f"Pool after live replace:\n{fi_show}")

    assert "/dev/mapper/braid-disk4" in fi_show, (
        f"New disk braid-disk4 missing from pool:\n{fi_show}"
    )
    assert "braid-disk2" not in fi_show, (
        f"Old disk braid-disk2 should be removed:\n{fi_show}"
    )
    assert "missing" not in fi_show.lower(), (
        f"Pool should have no missing devices:\n{fi_show}"
    )
    for name in ["braid-disk1", "braid-disk3"]:
        assert f"/dev/mapper/{name}" in fi_show, (
            f"{name} missing from pool:\n{fi_show}"
        )

    devid_count = fi_show.count("devid")
    assert devid_count == 3, f"Expected 3 devices, got {devid_count}:\n{fi_show}"

    df_output = machine.succeed("btrfs fi df /mnt/storage")
    assert "RAID1" in df_output, f"Expected RAID1 profile:\n{df_output}"

with subtest("Old disk LUKS mapper closed after live replace"):
    machine.fail("test -e /dev/mapper/braid-disk2")

with subtest("Data intact after live replace"):
    content = machine.succeed("cat /mnt/storage/precious.txt").strip()
    assert content == "important data", f"Expected 'important data', got '{content}'"

with subtest("Disk map updated after live replace"):
    dm = read_disk_map()
    assert "disk2" not in dm["disks"], f"disk2 still in map: {dm}"
    assert "disk4" in dm["disks"], f"disk4 missing from map: {dm}"
    for name in ["disk1", "disk3"]:
        assert name in dm["disks"], f"{name} missing from map: {dm}"

# --- Phase 2: Validation errors ---

with subtest("--missing-id rejected for live disk"):
    (status, output) = machine.execute(
        replace_cmd("disk1", "disk3", extra="--missing-id 99") + " 2>&1"
    )
    assert status != 0, f"Expected failure, got exit 0: {output}"
    assert "--missing-id" in output, f"Expected --missing-id error:\n{output}"

with subtest("Mixed state: simulate dead disk, then live replace fails"):
    # Close disk3 mapper to simulate missing device
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup close braid-disk3")
    machine.succeed("mount -o degraded /dev/mapper/braid-disk1 /mnt/storage")

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "missing" in fi_show.lower(), f"Expected missing device:\n{fi_show}"

    (status, output) = machine.execute(replace_cmd("disk1", "disk3") + " 2>&1")
    assert status != 0, f"Expected failure for mixed state, got exit 0: {output}"
    assert "missing" in output.lower(), f"Expected mention of missing devices:\n{output}"
    assert "remove-missing" in output, f"Expected remove-missing guidance:\n{output}"

machine.shutdown()
