# NOTE: I don't remember what I was trying to test here
#
# Repro: degraded RAID1 writes create single-profile block groups
#
# Intent: Prove that btrfs allocates single-profile (non-redundant) block
# groups for new writes when a 2-disk RAID1 loses one disk and is mounted
# degraded — leaving only 1 surviving disk, not enough for RAID1.
#
# Why it exists: This is a subtle data-safety pitfall. After losing a disk
# and mounting degraded, the pool *looks* like RAID1 but new data has zero
# redundancy. Users who replace the failed disk without rebalancing have
# unprotected data sitting on a "RAID1" pool. braid must rebalance after
# every disk replacement.
#
# Scenario: 2-disk LUKS+btrfs RAID1 pool (minimum for RAID1). Kill one
# disk, mount degraded with only 1 surviving disk. Write new data. btrfs
# fi df reveals single-profile block groups alongside RAID1. Then replace
# the dead disk with a spare, rebalance, and confirm everything is back
# to RAID1.
#
# Note: A 3-disk RAID1 losing 1 disk still has 2 survivors — enough for
# RAID1 — and would NOT trigger this behavior. The pitfall only appears
# when the surviving disk count drops below the RAID1 minimum of 2.

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
luks_format = "--batch-mode --key-file=- --pbkdf pbkdf2 --pbkdf-force-iterations 1000"

# --- Phase 1: Setup — LUKS format + open disk1-disk2, create RAID1 pool ---

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

# --- Phase 2: Baseline — write data, confirm pure RAID1 profile ---

with subtest("Baseline: only RAID1 block groups exist"):
    machine.succeed("dd if=/dev/urandom of=/mnt/storage/baseline.bin bs=1M count=50")
    machine.succeed("sync")

    fi_df = machine.succeed("btrfs fi df /mnt/storage")
    print(f"Baseline btrfs fi df:\n{fi_df}")
    assert "Data, RAID1" in fi_df, f"Expected 'Data, RAID1' in:\n{fi_df}"
    assert "Data, single" not in fi_df, f"Unexpected 'Data, single' in baseline:\n{fi_df}"

# --- Phase 3: Simulate disk death — close disk2, remount degraded ---

with subtest("Simulate disk2 death and mount degraded"):
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup luksClose disk2")
    machine.succeed("mount -o degraded /dev/mapper/disk1 /mnt/storage")

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print(f"Pool after disk2 death:\n{fi_show}")

# --- Phase 4: Write new data in degraded mode ---

with subtest("Write new data while degraded"):
    # Write enough to overflow existing RAID1 block groups and force btrfs
    # to allocate new block groups — with only 1 disk, these will be single
    machine.succeed("dd if=/dev/urandom of=/mnt/storage/degraded-write.bin bs=1M count=100")
    machine.succeed("sync")

# --- Phase 5: Core assertion — single-profile block groups appeared ---

with subtest("Degraded writes created single-profile block groups"):
    fi_df = machine.succeed("btrfs fi df /mnt/storage")
    print(f"After degraded writes btrfs fi df:\n{fi_df}")
    assert "Data, RAID1" in fi_df, f"Expected 'Data, RAID1' in:\n{fi_df}"
    assert "Data, single" in fi_df, \
        f"Expected 'Data, single' after degraded writes, but got:\n{fi_df}"

# --- Phase 6: Replace dead disk + rebalance restores pure RAID1 ---

with subtest("Replace dead disk with disk3 and rebalance to restore RAID1"):
    # LUKS format + open the replacement disk
    dev3 = "/dev/disk/by-id/virtio-disk3"
    machine.succeed(f"echo -n '{passphrase}' | cryptsetup luksFormat {luks_format} {dev3}")
    machine.succeed(f"echo -n '{passphrase}' | cryptsetup luksOpen --key-file=- {dev3} disk3")

    # Add replacement FIRST — can't remove missing from a 2-device RAID1
    # without going below the minimum device count
    machine.succeed("btrfs device add /dev/mapper/disk3 /mnt/storage")

    # Now remove the missing device reference (3 devices → 2 is fine)
    machine.succeed("btrfs device remove missing /mnt/storage")

    # Rebalance to convert everything back to RAID1
    machine.succeed("btrfs balance start -dconvert=raid1 -mconvert=raid1 /mnt/storage")

    fi_df = machine.succeed("btrfs fi df /mnt/storage")
    print(f"After rebalance btrfs fi df:\n{fi_df}")
    assert "Data, RAID1" in fi_df, f"Expected 'Data, RAID1' after rebalance:\n{fi_df}"
    assert "Data, single" not in fi_df, \
        f"Expected no 'Data, single' after rebalance, but got:\n{fi_df}"

machine.shutdown()
