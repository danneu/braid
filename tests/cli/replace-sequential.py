# Test: two sequential replacements in a 2-disk pool
#
# Intent:
# - What behavior this test (tries to) verify.
#   - Two back-to-back `braid replace` operations in a 2-disk pool both
#     succeed. After replacing disk1→disk3 and disk2→disk4, the pool contains
#     only disk3+disk4, both old LUKS mappers are closed, data is intact, and
#     the pool membership reflects the final state.
#
# Why it exists:
# - What risk/regression this protects against.
#   - The first replace may leave residual state (pool membership entries, pool
#     topology, LUKS mapper state) that causes the second replace to fail.
#     This is the actual migration workflow users follow when upgrading all
#     drives in a pool.
#
# Scenario:
# - Real-world situation this models.
#   - Operator upgrades from 2x old drives to 2x new drives, replacing them
#     one at a time without downtime.

import json

start_all()
machine.wait_for_unit("multi-user.target")

import shlex

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def read_pool():
    raw = machine.succeed("cat /var/lib/braid/pool.json")
    return json.loads(raw)


def add_cmd(name):
    passphrase_q = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {passphrase_q} | "
        f"braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 {name}=/dev/disk/by-id/virtio-{name} --passphrase-stdin --yes"
    )


def replace_cmd(old, new, extra=""):
    passphrase_q = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {passphrase_q} | "
        f"braid replace --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 --old {old} --new {new}=/dev/disk/by-id/virtio-{new} --passphrase-stdin --yes {extra}"
    )


# --- Phase 0: Build 2-drive pool ---

with subtest("Setup: build 2-drive pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    for name in ["braid-disk1", "braid-disk2"]:
        assert f"/dev/mapper/{name}" in fi_show, f"{name} missing:\n{fi_show}"

    df_output = machine.succeed("btrfs fi df /mnt/storage")
    assert "RAID1" in df_output, f"Expected RAID1 profile:\n{df_output}"

    machine.succeed("echo 'important data' > /mnt/storage/precious.txt")
    machine.succeed("sync")

# --- Phase 1: Replace disk1 → disk3 ---

with subtest("First replace: disk1 → disk3"):
    result = machine.succeed(replace_cmd("disk1", "disk3"))
    print(f"First replace output:\n{result}")

with subtest("Pool healthy after first replace"):
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print(f"Pool after first replace:\n{fi_show}")

    assert "/dev/mapper/braid-disk3" in fi_show, f"disk3 missing:\n{fi_show}"
    assert "/dev/mapper/braid-disk2" in fi_show, f"disk2 missing:\n{fi_show}"
    assert "braid-disk1" not in fi_show, f"disk1 should be gone:\n{fi_show}"
    assert "missing" not in fi_show.lower(), f"No missing devices expected:\n{fi_show}"

    devid_count = fi_show.count("devid")
    assert devid_count == 2, f"Expected 2 devices, got {devid_count}:\n{fi_show}"

    df_output = machine.succeed("btrfs fi df /mnt/storage")
    assert "RAID1" in df_output, f"Expected RAID1:\n{df_output}"

with subtest("Data intact after first replace"):
    content = machine.succeed("cat /mnt/storage/precious.txt").strip()
    assert content == "important data", f"Got '{content}'"

with subtest("Old mapper closed after first replace"):
    machine.fail("test -e /dev/mapper/braid-disk1")

with subtest("Pool membership correct after first replace"):
    pm = read_pool()
    assert "disk1" not in pm["disks"], f"disk1 still in pool: {pm}"
    assert "disk3" in pm["disks"], f"disk3 missing from pool: {pm}"
    assert "disk2" in pm["disks"], f"disk2 missing from pool: {pm}"

# --- Phase 2: Replace disk2 → disk4 ---

with subtest("Second replace: disk2 → disk4"):
    result = machine.succeed(replace_cmd("disk2", "disk4"))
    print(f"Second replace output:\n{result}")

with subtest("Pool healthy after second replace"):
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print(f"Pool after second replace:\n{fi_show}")

    assert "/dev/mapper/braid-disk3" in fi_show, f"disk3 missing:\n{fi_show}"
    assert "/dev/mapper/braid-disk4" in fi_show, f"disk4 missing:\n{fi_show}"
    assert "braid-disk2" not in fi_show, f"disk2 should be gone:\n{fi_show}"
    assert "braid-disk1" not in fi_show, f"disk1 should be gone:\n{fi_show}"
    assert "missing" not in fi_show.lower(), f"No missing devices expected:\n{fi_show}"

    devid_count = fi_show.count("devid")
    assert devid_count == 2, f"Expected 2 devices, got {devid_count}:\n{fi_show}"

    df_output = machine.succeed("btrfs fi df /mnt/storage")
    assert "RAID1" in df_output, f"Expected RAID1:\n{df_output}"

with subtest("Data intact after second replace"):
    content = machine.succeed("cat /mnt/storage/precious.txt").strip()
    assert content == "important data", f"Got '{content}'"

with subtest("Both old mappers closed"):
    machine.fail("test -e /dev/mapper/braid-disk1")
    machine.fail("test -e /dev/mapper/braid-disk2")

with subtest("Pool membership reflects final state"):
    pm = read_pool()
    assert "disk1" not in pm["disks"], f"disk1 still in pool: {pm}"
    assert "disk2" not in pm["disks"], f"disk2 still in pool: {pm}"
    assert "disk3" in pm["disks"], f"disk3 missing from pool: {pm}"
    assert "disk4" in pm["disks"], f"disk4 missing from pool: {pm}"

machine.shutdown()
