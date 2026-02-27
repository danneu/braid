# Repro: degraded 3-disk RAID1 — do new writes allocate single or RAID1?
#
# Intent: Determine whether btrfs allocates RAID1 or single-profile block
# groups when a 3-disk RAID1 loses 1 disk and is mounted degraded with
# 2 surviving disks.
#
# Why it exists: The 2-disk variant (degraded-writes-single) proved that
# losing 1 of 2 disks forces single-profile allocations — the surviving
# disk count (1) is below the RAID1 minimum of 2. But what about 3 disks
# losing 1? The 2 survivors *should* be enough for RAID1, but degraded
# mode might still fall back to single. This test finds out.
#
# Scenario: 3-disk LUKS+btrfs RAID1 pool. Kill disk3, mount degraded with
# disk1+disk2 still alive. Write new data. Check btrfs fi df to see whether
# the new block groups are RAID1 or single.

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
luks_format = "--batch-mode --key-file=- --pbkdf pbkdf2 --pbkdf-force-iterations 1000"


# --- Phase 1: Setup — LUKS format + open all 3 disks, create RAID1 pool ---

with subtest("Setup: create 3-drive LUKS + btrfs RAID1 pool"):
    for name in ["disk1", "disk2", "disk3"]:
        dev = f"/dev/disk/by-id/virtio-{name}"
        machine.succeed(f"echo -n '{passphrase}' | cryptsetup luksFormat {luks_format} {dev}")
        machine.succeed(f"echo -n '{passphrase}' | cryptsetup luksOpen --key-file=- {dev} {name}")

    machine.succeed(
        "mkfs.btrfs -f -d raid1 -m raid1"
        " /dev/mapper/disk1"
        " /dev/mapper/disk2"
        " /dev/mapper/disk3"
    )
    machine.succeed("mkdir -p /mnt/storage")
    machine.succeed("mount /dev/mapper/disk1 /mnt/storage")

# --- Phase 2: Baseline — write data, confirm pure RAID1 profile ---

with subtest("Baseline: only RAID1 block groups exist"):
    util.write_file_mib("/mnt/storage/baseline.bin", 100)

    fi_df = machine.succeed("btrfs fi df /mnt/storage")
    print(f"Baseline btrfs fi df:\n{fi_df}")
    assert "Data, RAID1" in fi_df, f"Expected 'Data, RAID1' in:\n{fi_df}"
    assert "Data, single" not in fi_df, f"Unexpected 'Data, single' in baseline:\n{fi_df}"

# --- Phase 3: Simulate disk death — close disk3, remount degraded ---

with subtest("Simulate disk3 death and mount degraded"):
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup luksClose disk3")
    machine.succeed("mount -o degraded /dev/mapper/disk1 /mnt/storage")

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print(f"Pool after disk3 death:\n{fi_show}")

# --- Phase 4: Write new data in degraded mode ---

with subtest("Write new data while degraded"):
    # Write enough to overflow existing RAID1 block groups and force btrfs
    # to allocate new block groups — with 2 surviving disks, will btrfs
    # allocate RAID1 or single?
    util.write_file_mib("/mnt/storage/degraded-write.bin", 100)

# --- Phase 5: Core observation — what profile did the new blocks get? ---

with subtest("Check block group profiles after degraded writes"):
    fi_df = machine.succeed("btrfs fi df /mnt/storage")
    print(f"After degraded writes btrfs fi df:\n{fi_df}")

    # Log the result clearly — this is the whole point of the test
    has_raid1 = "Data, RAID1" in fi_df
    has_single = "Data, single" in fi_df

    if has_single:
        print("RESULT: degraded 3-disk RAID1 (2 survivors) DOES create single-profile blocks")
    elif has_raid1 and not has_single:
        print("RESULT: degraded 3-disk RAID1 (2 survivors) keeps allocating RAID1 blocks")
    else:
        print(f"RESULT: unexpected profile mix: {fi_df}")

    # Assert our observation so the test will fail if we're ever wrong
    assert has_raid1, f"Expected degraded writes to keep RAID1 profile:\n{fi_df}"
    assert not has_single, f"Did not expect single-profile blocks after degraded writes:\n{fi_df}"

    # Also dump usage for extra context
    usage = machine.succeed("btrfs fi usage /mnt/storage")
    print(f"btrfs fi usage:\n{usage}")

machine.shutdown()
