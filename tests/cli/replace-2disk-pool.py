# Test: replace in a 2-disk pool
#
# Intent:
# - What behavior this test (tries to) verify.
#   - `braid replace` works correctly in a 2-disk RAID1 pool. After the
#     replace completes, the pool has exactly 2 devices, RAID1 profile is
#     intact, data is preserved, and the old disk's LUKS mapper is closed.
#
# Why it exists:
# - What risk/regression this protects against.
#   - The existing replace-live-disk test uses a 3-drive pool. A 2-disk pool
#     is the most common real-world setup and has a different topology
#     transition (2→3→2 vs 3→4→3).
#
# Scenario:
# - Real-world situation this models.
#   - Operator with a typical 2-drive NAS replaces one drive with a new one.

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
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid add {name}=/dev/disk/by-id/virtio-{name} --passphrase-stdin --yes"
    )


def replace_cmd(old, new, extra=""):
    passphrase_q = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {passphrase_q} | "
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid replace --old {old} --new {new}=/dev/disk/by-id/virtio-{new} --passphrase-stdin --yes {extra}"
    )


# --- Phase 0: Build 2-drive RAID1 pool ---

with subtest("Setup: build 2-drive pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    for name in ["braid-disk1", "braid-disk2"]:
        assert f"/dev/mapper/{name}" in fi_show, f"{name} missing:\n{fi_show}"

    devid_count = fi_show.count("devid")
    assert devid_count == 2, f"Expected 2 devices, got {devid_count}:\n{fi_show}"

    df_output = machine.succeed("btrfs fi df /mnt/storage")
    assert "RAID1" in df_output, f"Expected RAID1 profile:\n{df_output}"

    machine.succeed("echo 'important data' > /mnt/storage/precious.txt")
    machine.succeed("sync")

# --- Phase 1: Replace disk1 with disk3 ---

with subtest("Replace disk1 with disk3 in 2-disk pool"):
    result = machine.succeed(replace_cmd("disk1", "disk3"))
    print(f"braid replace output:\n{result}")

with subtest("Pool has exactly 2 devices after replace"):
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print(f"Pool after replace:\n{fi_show}")

    devid_count = fi_show.count("devid")
    assert devid_count == 2, (
        f"Expected exactly 2 devices after replace, got {devid_count}:\n{fi_show}"
    )

    assert "/dev/mapper/braid-disk3" in fi_show, (
        f"New disk braid-disk3 missing from pool:\n{fi_show}"
    )
    assert "/dev/mapper/braid-disk2" in fi_show, (
        f"Remaining disk braid-disk2 missing from pool:\n{fi_show}"
    )
    assert "braid-disk1" not in fi_show, (
        f"Old disk braid-disk1 should be removed:\n{fi_show}"
    )
    assert "missing" not in fi_show.lower(), (
        f"Pool should have no missing devices:\n{fi_show}"
    )

with subtest("RAID1 profile intact after replace"):
    df_output = machine.succeed("btrfs fi df /mnt/storage")
    assert "RAID1" in df_output, f"Expected RAID1 profile:\n{df_output}"

with subtest("Old disk LUKS mapper closed"):
    machine.fail("test -e /dev/mapper/braid-disk1")

with subtest("Data intact after replace"):
    content = machine.succeed("cat /mnt/storage/precious.txt").strip()
    assert content == "important data", f"Expected 'important data', got '{content}'"

with subtest("Pool membership updated after replace"):
    pm = read_pool()
    assert "disk1" not in pm["disks"], f"disk1 still in pool: {pm}"
    assert "disk3" in pm["disks"], f"disk3 missing from pool: {pm}"
    assert "disk2" in pm["disks"], f"disk2 missing from pool: {pm}"

machine.shutdown()
