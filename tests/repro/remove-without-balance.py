# Repro: can btrfs device remove work on a 2-disk RAID1 without balance first?
#
# Intent: Determine whether `btrfs device remove` can remove a device from a
# 2-disk RAID1 pool without first converting the profile to single via
# `btrfs balance -dconvert=single -mconvert=single`.
#
# Why it exists: braid currently always balances raid1→single before removing
# the second-to-last device. If btrfs can handle the remove directly, the
# balance step is unnecessary — and skipping it matters for failing drives,
# where the balance reads from all devices (including the failing one).
#
# Scenario: 2-disk LUKS+btrfs RAID1 pool with data. Attempt to remove one
# device using only `btrfs device remove` (no prior balance). Observe whether
# btrfs accepts or rejects this, and capture the exact behavior.
#
# Results:
#
#   Phase | Mount              | Target  | Balance | Action                 | Result
#   ------+--------------------+---------+---------+------------------------+------------------------------
#   2     | mount              | present | no      | device remove          | Fails — raid1 minimum
#   3     | mount              | present | no      | device remove missing  | Fails — no missing device
#   4     | mount -o degraded  | missing | no      | device remove missing  | Fails — still raid1 profile
#   5     | mount -o degraded  | missing | yes     | device remove missing  | Succeeds — data intact

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
luks_format = "--batch-mode --key-file=- --pbkdf pbkdf2 --pbkdf-force-iterations 1000"

# --- Phase 1: Setup — 2-drive LUKS + btrfs RAID1 pool with data ---

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

    # Write data so the pool has chunks to deal with
    machine.succeed("dd if=/dev/urandom of=/mnt/storage/testfile.bin bs=1M count=20")
    machine.succeed("sync")

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print(f"Baseline pool:\n{fi_show}")

# --- Phase 2: btrfs device remove without balance — fails (raid1 minimum) ---

with subtest("btrfs device remove without balance fails — raid1 requires 2 devices"):
    machine.fail(
        "btrfs device remove /dev/mapper/disk2 /mnt/storage 2>&1"
    )

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "/dev/mapper/disk1" in fi_show, "disk1 missing after failed remove"
    assert "/dev/mapper/disk2" in fi_show, "disk2 missing after failed remove"

# --- Phase 3: btrfs device remove missing while both devices present — fails ---

with subtest("btrfs device remove missing fails — no device is actually missing"):
    machine.fail(
        "btrfs device remove missing /mnt/storage 2>&1"
    )

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "/dev/mapper/disk1" in fi_show, "disk1 missing after failed remove"
    assert "/dev/mapper/disk2" in fi_show, "disk2 missing after failed remove"

# --- Phase 4: Close disk2 LUKS, remount degraded, remove missing — still fails ---

with subtest("Degraded mount with missing device — remove missing still fails (raid1 minimum)"):
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup close disk2")
    machine.succeed("mount -o degraded /dev/mapper/disk1 /mnt/storage")

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print(f"Pool in degraded mode:\n{fi_show}")
    assert "missing" in fi_show.lower(), "disk2 should be missing"

    machine.succeed("cat /mnt/storage/testfile.bin > /dev/null")

    machine.fail(
        "btrfs device remove missing /mnt/storage 2>&1"
    )

# --- Phase 5: Balance raid1→single in degraded mode, then remove missing — succeeds ---

with subtest("Balance raid1→single in degraded mode succeeds"):
    machine.succeed(
        "btrfs balance start -f -dconvert=single -mconvert=single /mnt/storage"
    )

    df_output = machine.succeed("btrfs filesystem df /mnt/storage")
    print(f"Profile after balance:\n{df_output}")

with subtest("Remove missing succeeds after balance to single"):
    machine.succeed(
        "btrfs device remove missing /mnt/storage"
    )

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print(f"Final pool:\n{fi_show}")
    assert "missing" not in fi_show.lower(), "missing device should be gone"

    df_output = machine.succeed("btrfs filesystem df /mnt/storage")
    print(f"Final profile:\n{df_output}")

    machine.succeed("cat /mnt/storage/testfile.bin > /dev/null")
    print("Data integrity: OK")

machine.shutdown()
