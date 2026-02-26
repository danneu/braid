# Test: braid remove-missing ENOSPC hang (slow relocation failure)
#
# Intent: Verify that `braid remove-missing` rejects the operation when
# surviving devices have SOME free space but not enough to complete
# relocation — replicating the real-world scenario where btrfs starts
# relocating, partially succeeds, then hangs or eventually fails.
#
# Why it exists: The companion test (braid-remove-missing-enospc) covers
# the "instantly full" case where btrfs fails immediately. This test
# covers the more dangerous case: btrfs STARTS working, then gets stuck.
# On real hardware (slow USB drives), this hangs for hours before
# crashing the filesystem. In a VM, btrfs may cycle faster and fail
# rather than truly hang — the pre-flight check must catch both.
#
# Scenario: 3×4096MiB RAID1 pool. Fill adaptively until each device has
# ~500-800MiB free (enough for btrfs to begin relocation but not finish).
# Kill one disk, mount degraded, and attempt remove-missing with a
# timeout. Without the pre-flight check, btrfs either hangs (killed by
# timeout) or fails with a raw ENOSPC error after partial relocation.

import re
import shlex

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"

MIB = 1024 * 1024


def add_cmd(key):
    """Build a `braid add <key> --yes` command with env vars."""
    passphrase_q = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {passphrase_q} | "
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid add {key} --passphrase-stdin --yes"
    )


def get_device_unallocated():
    """Return list of (device_label, unallocated_bytes) for online devices."""
    raw = machine.succeed("btrfs device usage --raw /mnt/storage")
    devices = []
    current_dev = None
    for line in raw.splitlines():
        # Device header: "/dev/mapper/braid-disk1, ID: 1" or "<missing disk>, ID: 3"
        dev_match = re.match(r"^(\S.*?), ID:", line)
        if dev_match:
            current_dev = dev_match.group(1)
            continue
        # Unallocated line: "   Unallocated:          1074790400"
        unalloc_match = re.match(r"\s+Unallocated:\s+(-?\d+)", line)
        if unalloc_match and current_dev and "missing" not in current_dev.lower():
            devices.append((current_dev, int(unalloc_match.group(1))))
    return devices


# --- Phase 1: Build 3-drive RAID1 pool ---

with subtest("Setup: build 3-drive pool with braid add"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed(add_cmd("disk3"))

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    for name in ["braid-disk1", "braid-disk2", "braid-disk3"]:
        assert f"/dev/mapper/{name}" in fi_show, f"{name} missing:\n{fi_show}"

    df_output = machine.succeed("btrfs fi df /mnt/storage")
    assert "RAID1" in df_output, f"Expected RAID1 profile:\n{df_output}"

# --- Phase 2: Adaptive fill — target 300-800MiB free per device ---
# Write in 50MiB chunks (smaller than a btrfs block group) and check
# per-device unallocated space after each. Stop when the minimum
# unallocated across online devices drops below 800MiB.
#
# The goal: both surviving devices have meaningful free space (300-800MiB)
# so btrfs can START relocating block groups off the dead device. But not
# enough to finish — the dead device has ~2-3GiB of data while survivors
# have ~1-1.5GiB total free between them.

with subtest("Fill pool adaptively — leave some but not enough free space"):
    dev_usage = machine.succeed("btrfs device usage --raw /mnt/storage")
    print(f"Device usage before fill:\n{dev_usage}")

    for i in range(1, 200):
        (status, output) = machine.execute(
            f"dd if=/dev/zero of=/mnt/storage/fill{i} bs=1M count=50 2>&1"
        )
        machine.succeed("sync")
        if status != 0:
            print(f"Fill stopped at chunk {i} (write failed): {output}")
            break

        devices = get_device_unallocated()
        min_free = min(b for _, b in devices)
        min_free_mib = min_free / MIB
        print(f"After chunk {i}: unallocated = {[(d.split('/')[-1], b // MIB) for d, b in devices]} MiB")
        if min_free_mib < 800:
            print(f"Target reached at chunk {i}: min unallocated = {min_free_mib:.0f} MiB")
            break

    dev_usage = machine.succeed("btrfs device usage --raw /mnt/storage")
    print(f"Device usage after fill:\n{dev_usage}")

    # Verify we hit the target: all online devices have SOME free space
    devices = get_device_unallocated()
    min_free_mib = min(b for _, b in devices) / MIB
    assert min_free_mib > 50, \
        f"Overshot: a device has only {min_free_mib:.0f} MiB free, wanted >50"

# --- Phase 3: Simulate disk death, mount degraded ---

with subtest("Simulate disk3 death and mount degraded"):
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup close braid-disk3")
    machine.succeed("mount -o degraded /dev/mapper/braid-disk1 /mnt/storage")

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print(f"Pool after simulated death:\n{fi_show}")
    assert "missing" in fi_show.lower(), f"Expected missing device:\n{fi_show}"

    dev_usage = machine.succeed("btrfs device usage --raw /mnt/storage")
    print(f"Device usage after death:\n{dev_usage}")

# --- Phase 4: Assert remove-missing fails with space error ---
# Without the pre-flight check, btrfs starts relocating block groups from
# the dead device. With some free space on survivors, it may partially
# succeed before running out of space. On real hardware this hangs; in a
# VM it may fail after retries. Either way, the 60s timeout prevents the
# test from blocking forever.

with subtest("remove-missing rejects operation due to insufficient space"):
    (status, output) = machine.execute(
        "timeout 60 braid remove-missing --yes 2>&1"
    )
    assert status != 0, f"Expected failure, got exit 0: {output}"
    assert "not enough" in output.lower() or "free space" in output.lower(), \
        f"Expected pre-flight space error in output:\n{output}"

# --- Phase 5: Assert pool is unchanged ---

with subtest("Pool unchanged — still degraded but functional"):
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "missing" in fi_show.lower(), \
        f"Missing device should still be present:\n{fi_show}"

with subtest("Filesystem still writable (not forced read-only)"):
    machine.succeed("touch /mnt/storage/test-write")
    machine.succeed("rm /mnt/storage/test-write")

machine.shutdown()
