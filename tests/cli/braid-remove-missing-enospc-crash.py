# Test: braid remove-missing — ENOSPC pre-flight rejection (partial free space)
#
# Intent: Verify that `braid remove-missing --yes` rejects when surviving
# devices have some free space but not enough to absorb the dead device's
# allocations.
#
# Why it exists: This is the catastrophic failure mode. Without the
# pre-flight check, btrfs starts relocating block groups, partially
# succeeds, then hits ENOSPC mid-transaction and crashes the filesystem
# to read-only. On real hardware with slow USB drives, this hangs for
# hours before crashing. The pre-flight check prevents this.
#
# Scenario: 3×4096MiB RAID1 pool, adaptively filled until each device
# has ~500-800MiB free. Kill one disk, mount degraded. braid must reject
# before btrfs gets a chance to start relocating.

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

def get_missing_devid():
    """Get the devid of the missing device from btrfs fi show."""
    import re
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    m = re.search(r"devid\s+(\d+)\s+.*missing", fi_show, re.IGNORECASE)
    assert m, "No missing device found in:\n" + fi_show
    return m.group(1)

# --- Phase 4: braid remove-missing rejects with space error ---

missing_devid = get_missing_devid()

with subtest("braid remove-missing rejects due to insufficient space"):
    (status, output) = machine.execute(
        f"braid remove-missing --missing-id {missing_devid} --yes 2>&1"
    )
    print(f"braid remove-missing output (exit {status}):\n{output}")
    assert status != 0, f"Expected failure, got exit 0: {output}"

    output_lower = output.lower()
    assert "not enough space" in output_lower, \
        f"Expected 'not enough space' in error:\n{output}"

# --- Phase 5: Pool unchanged — still has missing device ---

with subtest("Pool still has missing device (unchanged)"):
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print(f"Pool after rejection:\n{fi_show}")
    assert "missing" in fi_show.lower(), \
        f"Expected pool to still show missing device:\n{fi_show}"

# --- Phase 6: Filesystem still writable ---

with subtest("Filesystem still writable after rejection"):
    machine.succeed("touch /mnt/storage/test-write")
    machine.succeed("rm /mnt/storage/test-write")

machine.shutdown()
