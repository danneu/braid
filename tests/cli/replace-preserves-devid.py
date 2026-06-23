# Test: btrfs replace preserves devid (TDD signal for fast replace path)
#
# Intent:
# - What behavior this test verifies.
#   - After `braid replace --old <live> --new <larger>`, the new device
#     inherits the old device's btrfs devid. This proves `btrfs replace start`
#     was used (not add+balance+remove, which assigns a new devid). Also
#     verifies that the new larger disk reports its full capacity and that
#     pool.json is keyed by the new disk's live LUKS UUID.
#
# Why it exists:
# - What risk/regression this protects against.
#   - If the code regresses to add+balance+remove, the devid changes (e.g.,
#     from 2 to 3). This test catches that regression. It also catches any
#     failure to resize the new device to its full capacity, or to persist the
#     replacement under the canonical LUKS UUID identity.
#
# Scenario:
# - Real-world situation this models.
#   - Operator upgrades from smaller to larger drives (e.g. 2x12TB to 2x20TB).
#     The new drive should keep the same devid and use its full capacity.

import json
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


def get_devid(mapper_name):
    """Extract the btrfs devid for a given mapper from `btrfs fi show`."""
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    for line in fi_show.splitlines():
        if mapper_name in line:
            m = re.search(r"devid\s+(\d+)", line)
            if m:
                return int(m.group(1))
    raise AssertionError(f"devid not found for {mapper_name} in:\n{fi_show}")


def get_device_size_mib(mapper_name):
    """Extract the reported size (in MiB) of a device from `btrfs fi show`."""
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    for line in fi_show.splitlines():
        if mapper_name in line:
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


# --- Phase 0: Build 2-drive RAID1 pool (512MB each) ---

with subtest("Setup: build 2-drive pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    for name in ["braid-disk1", "braid-disk2"]:
        assert f"/dev/mapper/{name}" in fi_show, f"{name} missing:\n{fi_show}"

    df_output = machine.succeed("btrfs fi df /mnt/storage")
    assert "RAID1" in df_output, f"Expected RAID1 profile:\n{df_output}"

    machine.succeed("dd if=/dev/urandom of=/mnt/storage/testfile.bin bs=1M count=20")
    machine.succeed("sync")
    machine.succeed("md5sum /mnt/storage/testfile.bin > /tmp/checksum.txt")

# --- Phase 1: Record disk2's devid before replace ---

with subtest("Record disk2 devid"):
    disk2_devid = get_devid("braid-disk2")
    print(f"disk2 devid before replace: {disk2_devid}")

    disk2_uuid = machine.succeed(
        "cryptsetup luksUUID /dev/disk/by-id/virtio-disk2"
    ).strip()
    print(f"disk2 LUKS UUID before replace: {disk2_uuid}")

    old_size = get_device_size_mib("braid-disk2")
    print(f"disk2 size before replace: {old_size:.1f} MiB")

# --- Phase 2: Replace disk2 (512MB) with disk3 (1024MB) ---

with subtest("Replace disk2 with larger disk3"):
    result = machine.succeed(replace_cmd("disk2", "disk3"))
    print(f"braid replace output:\n{result}")

# --- Phase 3: Assert devid preserved (THE KEY TDD SIGNAL) ---

with subtest("Devid preserved: disk3 has disk2's old devid"):
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print(f"Pool after replace:\n{fi_show}")

    disk3_devid = get_devid("braid-disk3")
    print(f"disk3 devid after replace: {disk3_devid}")

    assert disk3_devid == disk2_devid, (
        f"devid NOT preserved! disk2 had devid {disk2_devid}, "
        f"disk3 has devid {disk3_devid}. "
        f"This means btrfs replace was NOT used (add+balance+remove assigns new devid)."
    )

# --- Phase 4: Assert disk3 reports full capacity (resize worked) ---

with subtest("New disk reports full capacity after resize"):
    new_size = get_device_size_mib("braid-disk3")
    print(f"disk3 size after replace: {new_size:.1f} MiB (old disk2: {old_size:.1f} MiB)")

    ratio = new_size / old_size
    assert ratio > 1.5, (
        f"New disk should be significantly larger than old. "
        f"Got {new_size:.1f} MiB vs {old_size:.1f} MiB (ratio {ratio:.2f}x, expected >1.5x). "
        f"Resize may not have been called after btrfs replace."
    )

# --- Phase 5: Standard health checks ---

with subtest("Pool healthy: disk2 gone, disk3 present, RAID1"):
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

with subtest("Old disk LUKS mapper closed"):
    machine.fail("test -e /dev/mapper/braid-disk2")

with subtest("Data intact after replace"):
    machine.succeed("md5sum -c /tmp/checksum.txt")

with subtest("Pool membership updated"):
    pm = read_pool()
    assert "disk2" not in member_names(pm), f"disk2 still in pool: {pm}"
    assert "disk3" in member_names(pm), f"disk3 missing from pool: {pm}"
    assert "disk1" in member_names(pm), f"disk1 missing from pool: {pm}"
    disk3_uuid = machine.succeed(
        "cryptsetup luksUUID /dev/disk/by-id/virtio-disk3"
    ).strip()
    assert_member_keyed_by_uuid(pm, "disk3", disk3_uuid)
    assert disk2_uuid not in pm["disks"], (
        f"old disk2 UUID key {disk2_uuid} still present: {pm}"
    )

machine.shutdown()
