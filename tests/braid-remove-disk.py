# Test: braid remove / braid remove-missing lifecycle
#
# What: Tests the two-command split for disk removal:
#   - `braid remove <name>` only removes present disks from the pool
#   - `braid remove-missing` explicitly removes missing/dead devices
#
# Why: `braid remove` previously had a dangerous implicit fallback — if a disk
# wasn't found in the pool but btrfs reported missing devices, it silently
# performed `btrfs device remove missing`. A typo or stale name could cause a
# destructive action on an unrelated device. The fix: `braid remove` only
# removes present disks; removing missing devices is a separate explicit
# command `braid remove-missing`.
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


def remove_missing_cmd(extra=""):
    """Build a `braid remove-missing --yes` command."""
    return f"braid remove-missing --yes {extra}"


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

with subtest("Remove nonexistent disk fails with 'not found in pool'"):
    # 'nonexistent' is not in the pool and no missing devices exist,
    # so braid remove should fail with a clear error.
    (status, output) = machine.execute(remove_cmd("nonexistent"))
    assert status != 0, f"Expected failure, got exit 0: {output}"
    assert "not found in pool" in output, f"Expected 'not found in pool' in error:\n{output}"

# --- Phase 1b: remove-missing with no missing devices ---

with subtest("remove-missing fails when no devices are missing"):
    (status, output) = machine.execute(remove_missing_cmd())
    assert status != 0, f"Expected failure, got exit 0: {output}"
    assert "no missing" in output.lower(), f"Expected 'no missing' in error:\n{output}"

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

# --- Phase 4: Remove of a dead disk must fail ---

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

with subtest("braid remove disk3 fails for dead disk"):
    (status, output) = machine.execute(remove_cmd("disk3"))
    assert status != 0, f"Expected failure, got exit 0: {output}"
    assert "not found in pool" in output, f"Expected 'not found in pool' in error:\n{output}"
    assert "missing" in output.lower(), f"Expected mention of missing devices:\n{output}"
    assert "remove-missing" in output, f"Expected suggestion to use remove-missing:\n{output}"

with subtest("Pool unchanged after failed remove"):
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "missing" in fi_show.lower(), f"Missing device should still be present:\n{fi_show}"

# --- Phase 5: Explicit remove-missing succeeds ---

with subtest("remove-missing succeeds for dead disk"):
    machine.succeed(remove_missing_cmd())

with subtest("No missing devices after remove-missing"):
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print(f"Pool after remove-missing:\n{fi_show}")
    assert "missing" not in fi_show.lower(), f"Still has missing device:\n{fi_show}"

with subtest("Data intact after remove-missing"):
    content = machine.succeed("cat /mnt/storage/precious.txt").strip()
    assert content == "important data", f"Expected 'important data', got '{content}'"

# --- Phase 5b: Multi-missing disambiguation ---

with subtest("Rebuild pool: re-add disk2 (disk3 already removed)"):
    # After Phase 5, pool has disk1 + disk2. Re-add disk3 to get 3 disks.
    machine.succeed(add_cmd("disk3"))

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    for name in ["braid-disk1", "braid-disk2", "braid-disk3"]:
        assert f"/dev/mapper/{name}" in fi_show, f"{name} missing after rebuild:\n{fi_show}"

with subtest("Simulate 2 dead disks"):
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup close braid-disk2")
    machine.succeed("cryptsetup close braid-disk3")
    machine.succeed("mount -o degraded /dev/mapper/braid-disk1 /mnt/storage")

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print(f"Pool after 2 simulated deaths:\n{fi_show}")
    assert "missing" in fi_show.lower(), f"Expected missing devices:\n{fi_show}"

with subtest("remove-missing without --missing-id fails with disambiguation error"):
    (status, output) = machine.execute(remove_missing_cmd())
    assert status != 0, f"Expected failure, got exit 0: {output}"
    assert "multiple missing" in output.lower() or "missing-id" in output.lower(), \
        f"Expected disambiguation error:\n{output}"

with subtest("remove-missing with --missing-id succeeds for one"):
    # Get devids from btrfs fi show to find a valid missing devid.
    # The alive device is disk1; disk2 and disk3 are missing.
    # We need to figure out what devids the missing devices have.
    # btrfs fi show lists alive devices with devids; missing ones are implicit.
    # Use braid status --verbose to get devids, or parse fi show's Total devices.
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print(f"fi show before targeted remove:\n{fi_show}")

    # Parse out the devid of the alive device
    import re
    alive_devids = [int(m) for m in re.findall(r"devid\s+(\d+)\s+size", fi_show)]
    total_match = re.search(r"Total devices\s+(\d+)", fi_show)
    total_devices = int(total_match.group(1))

    # Missing devids are the ones in range(1, total+1) not in alive_devids
    all_devids = set(range(1, total_devices + 1))
    missing_devids = sorted(all_devids - set(alive_devids))
    print(f"Alive devids: {alive_devids}, missing devids: {missing_devids}")
    assert len(missing_devids) >= 2, f"Expected 2+ missing devids, got {missing_devids}"

    # Remove one specific missing device
    target_devid = missing_devids[0]
    machine.succeed(remove_missing_cmd(f"--missing-id {target_devid}"))

with subtest("One missing device removed, one remains"):
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print(f"Pool after targeted remove-missing:\n{fi_show}")
    total_match = re.search(r"Total devices\s+(\d+)", fi_show)
    total_after = int(total_match.group(1))
    alive_after = len(re.findall(r"devid\s+\d+\s+size", fi_show))
    remaining_missing = total_after - alive_after
    assert remaining_missing == 1, f"Expected 1 remaining missing, got {remaining_missing}:\n{fi_show}"

machine.shutdown()
