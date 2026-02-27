# Repro: btrfs replace start preserves the replaced device's devid
#
# Intent: Prove that `btrfs replace start` preserves the replaced device's
# devid, while the alternative add+balance+remove assigns a new devid. Also
# confirm that `btrfs filesystem resize <devid>:max` works after replace to
# use a larger disk's full capacity.
#
# Why it exists: The entire `braid replace` optimization plan uses devid
# preservation as its TDD signal. If this assumption is wrong in our kernel
# version, the plan's test design is invalid.
#
# Scenario: 2-disk LUKS+btrfs RAID1 pool. Replace one disk with a larger one
# via `btrfs replace start`. Verify devid is preserved and resize works.

import re

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
luks_format = "--batch-mode --key-file=- --pbkdf pbkdf2 --pbkdf-force-iterations 1000"

# --- Phase 1: Setup — LUKS format + open disk1 and disk2, create RAID1 pool ---

with subtest("Setup: create 2-drive LUKS + btrfs RAID1 pool"):
    for name in ["disk1", "disk2"]:
        dev = f"/dev/disk/by-id/virtio-{name}"
        machine.succeed(f"echo -n '{passphrase}' | cryptsetup luksFormat {luks_format} {dev}")
        machine.succeed(f"echo -n '{passphrase}' | cryptsetup luksOpen --key-file=- {dev} {name}")

    machine.succeed(
        "mkfs.btrfs -f -d raid1 -m raid1"
        " /dev/mapper/disk1"
        " /dev/mapper/disk2"
    )
    machine.succeed("mkdir -p /mnt/storage")
    machine.succeed("mount /dev/mapper/disk1 /mnt/storage")

# --- Phase 2: Baseline — write test data, record disk2's devid ---

with subtest("Baseline: write data and record disk2 devid"):
    machine.succeed("dd if=/dev/urandom of=/mnt/storage/testfile.bin bs=1M count=50")
    machine.succeed("sync")
    machine.succeed("md5sum /mnt/storage/testfile.bin > /tmp/checksum.txt")

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print(f"Baseline btrfs fi show:\n{fi_show}")

    # Parse disk2's devid from btrfs fi show output
    # Lines look like: "  devid    2 size 496.00MiB ... path /dev/mapper/disk2"
    disk2_devid = None
    for line in fi_show.splitlines():
        if "/dev/mapper/disk2" in line:
            m = re.search(r"devid\s+(\d+)", line)
            assert m, f"Could not parse devid from line: {line}"
            disk2_devid = m.group(1)
            break
    assert disk2_devid is not None, f"disk2 not found in btrfs fi show:\n{fi_show}"
    print(f"disk2 devid: {disk2_devid}")

    # Also capture disk2's reported size for later comparison
    disk2_size_match = re.search(r"devid\s+" + disk2_devid + r"\s+size\s+([\d.]+\w+)", fi_show)
    disk2_size_str = disk2_size_match.group(1) if disk2_size_match else "unknown"
    print(f"disk2 size before replace: {disk2_size_str}")

# --- Phase 3: LUKS format + open disk3 (the larger replacement) ---

with subtest("LUKS prep disk3"):
    dev3 = "/dev/disk/by-id/virtio-disk3"
    machine.succeed(f"echo -n '{passphrase}' | cryptsetup luksFormat {luks_format} {dev3}")
    machine.succeed(f"echo -n '{passphrase}' | cryptsetup luksOpen --key-file=- {dev3} disk3")

# --- Phase 4: Replace disk2 with disk3 via btrfs replace start ---

with subtest("Replace disk2 with disk3 via btrfs replace start"):
    machine.succeed(
        f"btrfs replace start -f -B {disk2_devid} /dev/mapper/disk3 /mnt/storage"
    )
    print("btrfs replace start completed successfully")

# --- Phase 5: Assert devid preserved — disk3 should have disk2's old devid ---

with subtest("Assert devid preserved after replace"):
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print(f"After replace btrfs fi show:\n{fi_show}")

    # disk3's mapper should appear at the same devid that disk2 had
    disk3_devid = None
    for line in fi_show.splitlines():
        if "/dev/mapper/disk3" in line:
            m = re.search(r"devid\s+(\d+)", line)
            assert m, f"Could not parse devid from line: {line}"
            disk3_devid = m.group(1)
            break
    assert disk3_devid is not None, f"disk3 not found in btrfs fi show:\n{fi_show}"
    assert disk3_devid == disk2_devid, \
        f"devid NOT preserved! disk2 had devid {disk2_devid}, disk3 has devid {disk3_devid}"
    print(f"CONFIRMED: disk3 inherited disk2's devid {disk2_devid}")

# --- Phase 6: Assert disk2 is gone ---

with subtest("Assert disk2 no longer in pool"):
    assert "/dev/mapper/disk2" not in fi_show, \
        f"disk2 still appears in btrfs fi show after replace:\n{fi_show}"
    print("CONFIRMED: disk2 is gone from pool")

# --- Phase 7: Resize to use disk3's full capacity ---

with subtest("Resize to use full capacity of larger disk3"):
    machine.succeed(f"btrfs filesystem resize {disk2_devid}:max /mnt/storage")

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print(f"After resize btrfs fi show:\n{fi_show}")

    # Parse disk3's new size — should be >700 MiB (it's a 1024 MiB disk with LUKS overhead)
    for line in fi_show.splitlines():
        if "/dev/mapper/disk3" in line:
            size_match = re.search(r"size\s+([\d.]+)(\w+)", line)
            assert size_match, f"Could not parse size from line: {line}"
            size_val = float(size_match.group(1))
            size_unit = size_match.group(2)
            print(f"disk3 size after resize: {size_val}{size_unit}")
            if "MiB" in size_unit:
                assert size_val > 700, \
                    f"Expected disk3 size >700 MiB after resize, got {size_val} MiB"
            elif "GiB" in size_unit:
                assert size_val > 0.7, \
                    f"Expected disk3 size >0.7 GiB after resize, got {size_val} GiB"
            print(f"CONFIRMED: disk3 grew to {size_val}{size_unit}")
            break

# --- Phase 8: Data intact ---

with subtest("Data intact after replace"):
    machine.succeed("md5sum -c /tmp/checksum.txt")
    print("CONFIRMED: test data intact after replace")

machine.shutdown()
