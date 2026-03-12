# Repro: soft RAID1 balance restores redundancy after degraded operation
#
# Intent: Prove that `btrfs balance start -dconvert=raid1,soft -mconvert=raid1,soft`
# restores RAID1 profiles for single-profile chunks created during degraded mode,
# without rewriting already-RAID1 chunks.
#
# Why it exists: braid's `maybe_restore_raid1()` uses the `,soft` flag after
# `remove-missing` and `replace` (missing path). This test anchors the feature
# in observed btrfs behavior — proving `,soft` is sufficient (not just the
# non-soft variant proven in degraded-writes-single.py).
#
# Scenario: 2-disk LUKS+btrfs RAID1 pool. Kill disk2, mount degraded, write
# new data (creates single-profile chunks). Add disk3 as replacement, remove
# missing, then run the soft balance. Assert all profiles are back to RAID1.

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
    machine.succeed("dd if=/dev/urandom of=/mnt/storage/degraded-write.bin bs=1M count=100")
    machine.succeed("sync")

# --- Phase 5: Core assertion — single-profile block groups appeared ---

with subtest("Degraded writes created single-profile block groups"):
    fi_df = machine.succeed("btrfs fi df /mnt/storage")
    print(f"After degraded writes btrfs fi df:\n{fi_df}")
    assert "Data, RAID1" in fi_df, f"Expected 'Data, RAID1' in:\n{fi_df}"
    assert "Data, single" in fi_df, \
        f"Expected 'Data, single' after degraded writes, but got:\n{fi_df}"

# --- Phase 6: Add replacement disk, remove missing, soft balance ---

with subtest("Replace dead disk with disk3, remove missing, soft balance restores RAID1"):
    # LUKS format + open the replacement disk
    dev3 = "/dev/disk/by-id/virtio-disk3"
    machine.succeed(f"echo -n '{passphrase}' | cryptsetup luksFormat {luks_format} {dev3}")
    machine.succeed(f"echo -n '{passphrase}' | cryptsetup luksOpen --key-file=- {dev3} disk3")

    # Add replacement — can't remove missing from a 2-device RAID1
    # without going below the minimum device count
    machine.succeed("btrfs device add /dev/mapper/disk3 /mnt/storage")

    # Remove the missing device reference
    machine.succeed("btrfs device remove missing /mnt/storage")

    # Soft balance — the flag combination braid uses
    machine.succeed(
        "btrfs balance start"
        " -dconvert=raid1,soft"
        " -mconvert=raid1,soft"
        " /mnt/storage"
    )

    fi_df = machine.succeed("btrfs fi df /mnt/storage")
    print(f"After soft balance btrfs fi df:\n{fi_df}")
    assert "Data, RAID1" in fi_df, f"Expected 'Data, RAID1' after soft balance:\n{fi_df}"
    assert "Data, single" not in fi_df, \
        f"Expected no 'Data, single' after soft balance, but got:\n{fi_df}"
    assert "Metadata, single" not in fi_df, \
        f"Expected no 'Metadata, single' after soft balance, but got:\n{fi_df}"

machine.shutdown()
