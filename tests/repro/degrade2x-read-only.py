# Myth-bust: "second degraded mount goes read-only" (DISPROVED)
#
# Intent: Disprove the common belief that mounting a btrfs RAID1 pool in
# degraded mode a second time makes it read-only. This was true before
# kernel 4.14 (device-level check) but is false on modern kernels (per-chunk
# check since 4.14). Single-profile chunks created during degraded operation
# live on the available device, so the per-chunk check passes (missing=0,
# tolerance=0 → OK).
#
# Why it exists: Documents a known myth so braid doesn't implement unnecessary
# workarounds. Also proves the REAL risk: data written while degraded gets
# single-profile chunks with zero redundancy, which is a silent data safety
# hazard that braid must detect and warn about.
#
# Scenario: 2-disk LUKS+btrfs RAID1. Kill one disk. Mount degraded, write
# enough data to force new single-profile chunk allocation, unmount. Mount
# degraded again — confirm it still comes up rw (myth busted). Confirm
# single-profile chunks exist (real risk proven).

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

# --- Phase 2: Baseline — write data, unmount cleanly ---

with subtest("Baseline: write data and unmount"):
    machine.succeed("dd if=/dev/urandom of=/mnt/storage/baseline.bin bs=1M count=50")
    machine.succeed("sync")
    machine.succeed("umount /mnt/storage")

# --- Phase 3: Simulate disk death — close disk2 ---

with subtest("Simulate disk2 death"):
    machine.succeed("cryptsetup luksClose disk2")

# --- Phase 4: First degraded mount — write enough to force single-profile chunks ---

with subtest("First degraded mount: write enough to create single-profile chunks"):
    machine.succeed("mount -o degraded /dev/mapper/disk1 /mnt/storage")

    fi_usage_before = machine.succeed("btrfs fi usage /mnt/storage")
    print(f"Usage before big write:\n{fi_usage_before}")

    # Write enough to exhaust pre-allocated raid1 data chunks and force
    # btrfs to allocate new single-profile chunks on the one remaining device.
    machine.succeed("dd if=/dev/urandom of=/mnt/storage/fill1.bin bs=1M count=400")
    machine.succeed("sync")

    fi_usage_after = machine.succeed("btrfs fi usage /mnt/storage")
    print(f"Usage after big write:\n{fi_usage_after}")

    # Verify single-profile data chunks were created — this is the real risk
    fi_df = machine.succeed("btrfs fi df /mnt/storage")
    print(f"Chunk profiles after degraded writes:\n{fi_df}")
    assert "Data, single" in fi_df, \
        f"Expected single-profile data chunks after degraded writes, but got:\n{fi_df}"

    machine.succeed("umount /mnt/storage")

# --- Phase 5: Second degraded mount — MYTH BUST: still comes up rw ---

with subtest("Second degraded mount: still rw (myth busted)"):
    machine.succeed("mount -o degraded /dev/mapper/disk1 /mnt/storage")

    # Confirm the mount is rw — this is the myth-bust
    mount_entry = machine.succeed("grep /mnt/storage /proc/mounts")
    print(f"Mount entry: {mount_entry}")
    assert "rw," in mount_entry, \
        f"Expected rw mount (modern kernel per-chunk check), but got:\n{mount_entry}"

    # Writes still succeed on second degraded mount
    machine.succeed("dd if=/dev/urandom of=/mnt/storage/degraded2.bin bs=1M count=1")
    machine.succeed("sync")

    # But single-profile chunks are still there — data has no redundancy
    fi_df = machine.succeed("btrfs fi df /mnt/storage")
    print(f"Chunk profiles on second degraded mount:\n{fi_df}")
    assert "Data, single" in fi_df, \
        "Single-profile chunks should persist across mounts"

    machine.succeed("umount /mnt/storage")

machine.shutdown()
