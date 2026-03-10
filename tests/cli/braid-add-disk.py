# Test: braid add lifecycle
#
# What: Runs `braid add <name> --yes` through its full lifecycle: first disk
# (creates pool), second disk (converts to RAID1), third disk (expands pool),
# validation errors, idempotent re-add, pre-formatted disk recovery, and fifth
# disk expansion.
#
# Why: `braid add` is the primary intent command for LUKS format + pool join.
# Every primitive has been proven in isolation (luks, btrfs-raid1, grow, shrink,
# heal, degrade). This test proves the intent CLI ties them together correctly.
#
# Dependencies: btrfs-grow1 (single -> RAID1 -> 3-drive works manually).

start_all()
machine.wait_for_unit("multi-user.target")

import shlex

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def add_cmd(key):
    """Build a `braid add <key> --yes` command with env vars."""
    passphrase_q = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {passphrase_q} | "
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid add {key} --passphrase-stdin --yes"
    )


# --- Phase 1: First disk (no pool) ---

with subtest("First disk creates single-drive pool"):
    machine.succeed(add_cmd("disk1"))

    # Pool is mounted
    machine.succeed("mountpoint -q /mnt/storage")

    # Single profile (only 1 drive)
    df_output = machine.succeed("btrfs fi df /mnt/storage")
    assert "Data, single" in df_output, f"Expected single profile:\n{df_output}"
    assert "Metadata, DUP" in df_output, f"Expected DUP metadata profile:\n{df_output}"

    # LUKS mapper exists with correct name (braid-<key>)
    machine.succeed("test -e /dev/mapper/braid-disk1")

    # Can write data
    machine.succeed("echo 'day one data' > /mnt/storage/day1.txt")
    machine.succeed("sync")

# --- Phase 2: Second disk (convert to RAID1) ---

with subtest("Second disk converts pool to RAID1"):
    machine.succeed(add_cmd("disk2"))

    df_output = machine.succeed("btrfs fi df /mnt/storage")
    assert "Data, RAID1" in df_output, f"Expected RAID1:\n{df_output}"

with subtest("Day 1 data survived RAID1 conversion"):
    content = machine.succeed("cat /mnt/storage/day1.txt").strip()
    assert content == "day one data", f"Expected 'day one data', got '{content}'"

with subtest("Write more data on RAID1"):
    machine.succeed("echo 'day two data' > /mnt/storage/day2.txt")
    machine.succeed("sync")

# --- Phase 3: Third disk (add to RAID1) ---

with subtest("Third disk expands RAID1 pool"):
    machine.succeed(add_cmd("disk3"))

    # All 3 mapper devices in pool
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    for name in ["braid-disk1", "braid-disk2", "braid-disk3"]:
        assert f"/dev/mapper/{name}" in fi_show, f"{name} missing:\n{fi_show}"

with subtest("All data survived third disk addition"):
    content1 = machine.succeed("cat /mnt/storage/day1.txt").strip()
    content2 = machine.succeed("cat /mnt/storage/day2.txt").strip()
    assert content1 == "day one data", f"Expected 'day one data', got '{content1}'"
    assert content2 == "day two data", f"Expected 'day two data', got '{content2}'"

# --- Phase 4: Validation errors ---

with subtest("Non-existent key fails add"):
    machine.fail(add_cmd("nonexistent"))

with subtest("Already-in-pool disk is a no-op (exit 0)"):
    machine.succeed(add_cmd("disk1"))

# --- Phase 5: Crash recovery — pre-formatted LUKS ---

with subtest("Crash recovery — pre-formatted LUKS completes add"):
    # Format disk4 as LUKS manually (simulating crash between luksFormat and add)
    dev = "/dev/disk/by-id/virtio-disk4"
    machine.succeed(
        f"echo -n '{passphrase}' | cryptsetup luksFormat --batch-mode --key-file=- "
        f"{luks_opts} {dev}"
    )

    # braid add should detect existing LUKS, skip luksFormat, open, and add to pool
    machine.succeed(add_cmd("disk4"))

    # Verify it was added to the pool
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "/dev/mapper/braid-disk4" in fi_show, f"braid-disk4 missing:\n{fi_show}"

# --- Phase 6: Fifth disk expands pool ---

with subtest("Fifth disk expands pool"):
    machine.succeed(add_cmd("disk5"))

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "/dev/mapper/braid-disk5" in fi_show, f"braid-disk5 missing:\n{fi_show}"
    devid_count = fi_show.count("devid")
    assert devid_count == 5, f"Expected 5 devices, got {devid_count}:\n{fi_show}"

with subtest("All data survived fifth disk addition"):
    content1 = machine.succeed("cat /mnt/storage/day1.txt").strip()
    content2 = machine.succeed("cat /mnt/storage/day2.txt").strip()
    assert content1 == "day one data", f"Expected 'day one data', got '{content1}'"
    assert content2 == "day two data", f"Expected 'day two data', got '{content2}'"

machine.shutdown()
