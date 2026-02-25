# Test: braid remove lifecycle
#
# What: Runs `braid remove <name> --yes` through its full lifecycle: build a
# 3-drive RAID1 pool, validate error cases, graceful remove of a present disk,
# redundancy-reducing remove, and remove-missing for a dead disk.
#
# Why: `braid remove` is the symmetric counterpart to `braid add`. It must
# handle both happy path (disk present, data migrates off) and failure path
# (disk gone, remove missing). Pool-authoritative — resolves against the live
# pool, not config.
#
# Dependencies: braid add (builds the test pool).

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def add_cmd(name):
    """Build a `braid add <name> --yes` command with env vars."""
    return (
        f"BRAID_PASSPHRASE='{passphrase}' "
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid add {name} --yes"
    )


def remove_cmd(name, extra=""):
    """Build a `braid remove <name> --yes` command."""
    return f"braid remove {name} --yes {extra}"


# --- Phase 0: Build 3-drive RAID1 pool ---

with subtest("Setup: build 3-drive pool with braid add"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed(add_cmd("disk3"))

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    for name in ["braid-disk1", "braid-disk2", "braid-disk3"]:
        assert f"/dev/mapper/{name}" in fi_show, f"{name} missing:\n{fi_show}"

    df_output = machine.succeed("btrfs fi df /mnt/storage")
    assert "RAID1" in df_output, f"Expected RAID1 profile:\n{df_output}"

    machine.succeed("echo 'important data' > /mnt/storage/precious.txt")
    machine.succeed("sync")

# --- Phase 1: Validation errors ---

with subtest("Remove nonexistent disk fails"):
    # 'nonexistent' is not in the pool and no missing devices exist,
    # so braid remove should fail.
    machine.fail(remove_cmd("nonexistent"))

# --- Phase 2: Graceful remove (disk3 present in pool) ---

with subtest("Graceful remove of disk3"):
    machine.succeed(remove_cmd("disk3"))

with subtest("disk3 gone from pool, disk1+disk2 remain, RAID1 profile"):
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print(f"Pool after graceful remove:\n{fi_show}")
    assert "braid-disk3" not in fi_show, f"disk3 still in pool:\n{fi_show}"
    for name in ["braid-disk1", "braid-disk2"]:
        assert f"/dev/mapper/{name}" in fi_show, f"{name} missing:\n{fi_show}"

    df_output = machine.succeed("btrfs fi df /mnt/storage")
    assert "RAID1" in df_output, f"Expected RAID1 profile:\n{df_output}"

with subtest("LUKS mapper closed after graceful remove"):
    machine.fail("test -e /dev/mapper/braid-disk3")

with subtest("Data intact after graceful remove"):
    content = machine.succeed("cat /mnt/storage/precious.txt").strip()
    assert content == "important data", f"Expected 'important data', got '{content}'"

# --- Phase 3: Redundancy warning ---
# Pool has disk1 + disk2. Removing disk2 leaves 1 disk (no redundancy).
# With --yes, the interactive redundancy confirmation is bypassed.

with subtest("Redundancy-reducing remove with --yes succeeds"):
    machine.succeed(remove_cmd("disk2"))

with subtest("Pool has 1 device after redundancy removal"):
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print(f"Pool after redundancy removal:\n{fi_show}")
    devid_count = fi_show.count("devid")
    assert devid_count == 1, f"Expected 1 device, got {devid_count}:\n{fi_show}"
    assert "braid-disk1" in fi_show, f"disk1 missing:\n{fi_show}"

with subtest("LUKS mapper closed after redundancy removal"):
    machine.fail("test -e /dev/mapper/braid-disk2")

with subtest("Data intact after redundancy removal"):
    content = machine.succeed("cat /mnt/storage/precious.txt").strip()
    assert content == "important data", f"Expected 'important data', got '{content}'"

# --- Phase 4: Remove-missing ---

with subtest("Rebuild pool: re-add disk2 and disk3"):
    machine.succeed(add_cmd("disk2"))
    machine.succeed(add_cmd("disk3"))

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    for name in ["braid-disk1", "braid-disk2", "braid-disk3"]:
        assert f"/dev/mapper/{name}" in fi_show, f"{name} missing after rebuild:\n{fi_show}"

    df_output = machine.succeed("btrfs fi df /mnt/storage")
    assert "RAID1" in df_output, f"Expected RAID1 after rebuild:\n{df_output}"

with subtest("Simulate disk3 death and mount degraded"):
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup close braid-disk3")
    machine.succeed("mount -o degraded /dev/mapper/braid-disk1 /mnt/storage")

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print(f"Pool after simulated death:\n{fi_show}")
    assert "missing" in fi_show.lower(), f"Expected missing device:\n{fi_show}"

with subtest("Remove-missing succeeds for dead disk3"):
    machine.succeed(remove_cmd("disk3"))

with subtest("No missing devices after remove-missing"):
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print(f"Pool after remove-missing:\n{fi_show}")
    assert "missing" not in fi_show.lower(), f"Still has missing device:\n{fi_show}"

with subtest("Data intact after remove-missing"):
    content = machine.succeed("cat /mnt/storage/precious.txt").strip()
    assert content == "important data", f"Expected 'important data', got '{content}'"

machine.shutdown()
