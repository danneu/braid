# Repro: btrfs device remove missing — partial relocation crash
#
# Intent: Reproduce and document btrfs failure mode 2. When surviving
# devices have SOME free space but not enough to complete relocation,
# btrfs starts moving block groups, partially succeeds, then hits ENOSPC
# mid-transaction. The transaction abort forces the filesystem read-only.
#
# Why it exists: This is the catastrophic failure mode. Unlike the
# instant-ENOSPC case (btrfs-remove-enospc), the filesystem is destroyed.
# On real hardware with slow USB drives, the same sequence takes hours
# before crashing. In a VM it takes ~40 seconds.
#
# Scenario: 3×4096MiB RAID1 pool, adaptively filled until each device
# has ~500-800MiB free. Kill one disk, mount degraded. btrfs starts
# relocating block groups from the dead device, succeeds on one, then
# crashes on the next when it runs out of space.

import re
import shlex

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"

MIB = 1024 * 1024


def add_cmd(key):
    passphrase_q = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {passphrase_q} | "
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid add {key}=/dev/disk/by-id/virtio-{key} --passphrase-stdin --yes"
    )


def get_device_unallocated():
    """Return list of (device_label, unallocated_bytes) for online devices."""
    raw = machine.succeed("btrfs device usage --raw /mnt/storage")
    devices = []
    current_dev = None
    for line in raw.splitlines():
        dev_match = re.match(r"^(\S.*?), ID:", line)
        if dev_match:
            current_dev = dev_match.group(1)
            continue
        unalloc_match = re.match(r"\s+Unallocated:\s+(-?\d+)", line)
        if unalloc_match and current_dev and "missing" not in current_dev.lower():
            devices.append((current_dev, int(unalloc_match.group(1))))
    return devices


# --- Phase 1: Build 3-drive RAID1 pool ---

with subtest("Setup: build 3-drive pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed(add_cmd("disk3"))

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    for name in ["braid-disk1", "braid-disk2", "braid-disk3"]:
        assert f"/dev/mapper/{name}" in fi_show, f"{name} missing:\n{fi_show}"

# --- Phase 2: Adaptive fill — target ~500-800MiB free per device ---
# Write in 50MiB chunks and stop when the minimum unallocated across
# devices drops below 800MiB. This leaves enough free space for btrfs
# to START relocating but not enough to finish.

with subtest("Fill pool adaptively"):
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
    print(f"Pool after death:\n{fi_show}")
    assert "missing" in fi_show.lower()

    dev_usage = machine.succeed("btrfs device usage --raw /mnt/storage")
    print(f"Device usage after death:\n{dev_usage}")

# --- Phase 4: btrfs device remove crashes the filesystem ---
# btrfs starts relocating, partially succeeds, then hits ENOSPC
# mid-transaction. Transaction abort → forced read-only.

with subtest("btrfs device remove missing crashes filesystem to read-only"):
    (status, output) = machine.execute(
        "timeout 120 btrfs device remove missing /mnt/storage 2>&1"
    )
    print(f"btrfs device remove output (exit {status}):\n{output}")
    assert status != 0, f"Expected failure, got exit 0: {output}"

# --- Phase 5: Filesystem is destroyed — forced read-only ---

with subtest("Filesystem is read-only after crash"):
    (status, _) = machine.execute("touch /mnt/storage/test-write 2>&1")
    assert status != 0, "Expected write to fail on read-only filesystem"

    # Confirm via dmesg
    dmesg = machine.succeed("dmesg | grep -i 'forced readonly' || true")
    print(f"dmesg forced readonly:\n{dmesg}")
    assert "forced readonly" in dmesg.lower(), \
        f"Expected 'forced readonly' in dmesg:\n{dmesg}"

machine.shutdown()
