# Test: braid remove-missing ENOSPC pre-flight check
#
# Intent: Verify that `braid remove-missing` rejects the operation when
# surviving devices don't have enough unallocated space to absorb relocation
# from the missing device.
#
# Why it exists: Without this check, `braid remove-missing` delegates to
# `btrfs device remove missing` which hangs for a long time, then crashes
# the filesystem to read-only. This test ensures braid detects the condition
# and fails with a clear error before invoking btrfs.
#
# Scenario: Models the real incident: 3×512MiB RAID1 pool, ~80% full, one
# drive dies. The remaining two drives have all data mirrored (RAID1) but
# not enough unallocated space for btrfs to relocate block groups off the
# dead device.

import shlex

start_all()
machine.wait_for_unit("multi-user.target")

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


# --- Phase 1: Build 3-drive RAID1 pool ---

with subtest("Setup: build 3-drive pool with braid add"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed(add_cmd("disk3"))

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    for name in ["braid-disk1", "braid-disk2", "braid-disk3"]:
        assert f"/dev/mapper/{name}" in fi_show, f"{name} missing:\n{fi_show}"

    df_output = machine.succeed("btrfs fi df /mnt/storage")
    assert "RAID1" in df_output, f"Expected RAID1 profile:\n{df_output}"

# --- Phase 2: Fill pool to ~80% ---

with subtest("Fill pool to constrain free space"):
    # Log space before fill
    usage_before = machine.succeed("btrfs fi usage /mnt/storage")
    print(f"Space before fill:\n{usage_before}")

    # RAID1 on 3×512MiB: effective capacity ~500MiB after LUKS+metadata overhead.
    # Fill aggressively — use machine.execute to tolerate ENOSPC on the last write.
    machine.succeed("dd if=/dev/zero of=/mnt/storage/fill1 bs=1M count=200 status=progress")
    machine.succeed("sync")
    machine.succeed("dd if=/dev/zero of=/mnt/storage/fill2 bs=1M count=200 status=progress")
    machine.succeed("sync")
    # This may ENOSPC — that's fine, we just want maximum fill
    machine.execute("dd if=/dev/zero of=/mnt/storage/fill3 bs=1M count=200 status=progress")
    machine.succeed("sync")

    # Log space after fill for debugging
    usage_after = machine.succeed("btrfs fi usage /mnt/storage")
    print(f"Space after fill:\n{usage_after}")
    dev_usage = machine.succeed("btrfs device usage --raw /mnt/storage")
    print(f"Device usage after fill:\n{dev_usage}")

# --- Phase 3: Simulate disk death, mount degraded ---

with subtest("Simulate disk3 death and mount degraded"):
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup close braid-disk3")
    machine.succeed("mount -o degraded /dev/mapper/braid-disk1 /mnt/storage")

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print(f"Pool after simulated death:\n{fi_show}")
    assert "missing" in fi_show.lower(), f"Expected missing device:\n{fi_show}"

# --- Phase 4: Assert remove-missing fails with space error ---

with subtest("remove-missing rejects operation due to insufficient space"):
    # Use timeout to prevent hang if the check isn't implemented yet and
    # btrfs device remove gets invoked (it would hang for a very long time).
    (status, output) = machine.execute(
        "timeout 120 braid remove-missing --yes 2>&1"
    )
    assert status != 0, f"Expected failure, got exit 0: {output}"
    assert "not enough" in output.lower() or "free space" in output.lower(), \
        f"Expected space error in output:\n{output}"

# --- Phase 5: Assert pool is unchanged ---

with subtest("Pool unchanged — still degraded but functional"):
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "missing" in fi_show.lower(), \
        f"Missing device should still be present:\n{fi_show}"

with subtest("Filesystem still writable (not forced read-only)"):
    machine.succeed("touch /mnt/storage/test-write")
    machine.succeed("rm /mnt/storage/test-write")

machine.shutdown()
