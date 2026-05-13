# Test: replace a disk with a larger one
#
# Intent:
# - What behavior this test (tries to) verify.
#   - `braid replace --old <small> --new <large>` succeeds and btrfs reports
#     the full capacity of the larger new disk, not capped at the old size.
#
# Why it exists:
# - What risk/regression this protects against.
#   - This is the exact user migration scenario: upgrading from smaller to
#     larger drives (e.g. 2x12TB → 2x20TB). If btrfs or braid caps the new
#     device at the old size, the user silently loses capacity.
#
# Scenario:
# - Real-world situation this models.
#   - Operator buys larger drives and replaces pool members one at a time.
#     After replacing a 512MB disk with a 1024MB disk, the pool should report
#     the new disk at its full ~1GiB size.

import json


def member_names(pool):
    return {member["name"] for member in pool["disks"].values()}


def member(pool, name):
    for entry in pool["disks"].values():
        if entry["name"] == name:
            return entry
    raise AssertionError(f"{name} missing from pool.json: {pool}")
import re

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


def get_device_size_mib(mapper_name):
    """Extract the reported size (in MiB) of a device from `btrfs fi show`."""
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    for line in fi_show.splitlines():
        if mapper_name in line:
            # Match sizes like "1006.00MiB" or "1.00GiB"
            m = re.search(r"size\s+([\d.]+)([A-Za-z]+)", line)
            if m:
                val = float(m.group(1))
                unit = m.group(2)
                if unit == "GiB":
                    return val * 1024
                elif unit == "MiB":
                    return val
                elif unit == "TiB":
                    return val * 1024 * 1024
    raise AssertionError(f"size not found for {mapper_name} in:\n{fi_show}")


# --- Phase 0: Build 2-drive pool with 512MB disks ---

with subtest("Setup: build 2-drive pool (512MB each)"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    for name in ["braid-disk1", "braid-disk2"]:
        assert f"/dev/mapper/{name}" in fi_show, f"{name} missing:\n{fi_show}"

    df_output = machine.succeed("btrfs fi df /mnt/storage")
    assert "RAID1" in df_output, f"Expected RAID1 profile:\n{df_output}"

    machine.succeed("echo 'important data' > /mnt/storage/precious.txt")
    machine.succeed("sync")

    # Record size of a 512MB disk for comparison
    old_size = get_device_size_mib("braid-disk2")
    print(f"Old disk2 size: {old_size} MiB")

# --- Phase 1: Replace disk2 (512MB) with disk3 (1024MB) ---

with subtest("Replace disk2 (512MB) with disk3 (1024MB)"):
    result = machine.succeed(replace_cmd("disk2", "disk3"))
    print(f"braid replace output:\n{result}")

with subtest("New disk reports full capacity (significantly larger than old disk)"):
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print(f"Pool after replace:\n{fi_show}")

    new_size = get_device_size_mib("braid-disk3")
    print(f"New disk3 size: {new_size} MiB (old disk2 was {old_size} MiB)")

    # The new disk (1024MB raw) should be ~1.8x+ the old disk (512MB raw)
    # after LUKS overhead. If capped at old size, ratio would be ~1.0.
    ratio = new_size / old_size
    assert ratio > 1.5, (
        f"New disk should be significantly larger than old. "
        f"Got {new_size:.1f} MiB vs {old_size:.1f} MiB (ratio {ratio:.2f}x, expected >1.5x). "
        f"Disk may be capped at old size."
    )

with subtest("Pool healthy after larger-disk replace"):
    fi_show = machine.succeed("btrfs fi show /mnt/storage")

    assert "/dev/mapper/braid-disk3" in fi_show, (
        f"braid-disk3 missing from pool:\n{fi_show}"
    )
    assert "braid-disk2" not in fi_show, (
        f"Old disk braid-disk2 should be removed:\n{fi_show}"
    )
    assert "missing" not in fi_show.lower(), (
        f"Pool should have no missing devices:\n{fi_show}"
    )

    devid_count = fi_show.count("devid")
    assert devid_count == 2, f"Expected 2 devices, got {devid_count}:\n{fi_show}"

    df_output = machine.succeed("btrfs fi df /mnt/storage")
    assert "RAID1" in df_output, f"Expected RAID1 profile:\n{df_output}"

with subtest("Data intact after larger-disk replace"):
    content = machine.succeed("cat /mnt/storage/precious.txt").strip()
    assert content == "important data", f"Expected 'important data', got '{content}'"

with subtest("Pool membership updated after larger-disk replace"):
    pm = read_pool()
    assert "disk2" not in member_names(pm), f"disk2 still in pool: {pm}"
    assert "disk3" in member_names(pm), f"disk3 missing from pool: {pm}"
    assert "disk1" in member_names(pm), f"disk1 missing from pool: {pm}"

machine.shutdown()
