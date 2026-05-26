# Test: multi-disk braid add
#
# What: Tests `braid add disk1 disk2` (multi-disk add) through three scenarios:
# (1) bootstrap a new pool with 2 disks in one command → RAID1 from the start;
# (2) add 2 more disks to existing pool in one command → one balance;
# (3) single-disk add to an existing pool.
#
# Why: Multi-disk add is the recommended way to start a pool. It uses
# mkfs.btrfs -d raid1 -m raid1 to create the filesystem already in RAID1,
# avoiding the full-data-rewrite balance that single→RAID1 conversion requires.
#
# Dependencies: braid-add-disk (single-disk add lifecycle works).

start_all()
machine.wait_for_unit("multi-user.target")

import shlex

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def add_cmd(*keys):
    """Build a `braid add key1=by_id1 key2=by_id2 ... --yes` command with LUKS format args."""
    passphrase_q = shlex.quote(passphrase)
    disk_args = " ".join(f"{k}=/dev/disk/by-id/virtio-{k}" for k in keys)
    return (
        f"printf '%s\\n' {passphrase_q} | "
        f"braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 {disk_args} --passphrase-stdin --yes"
    )


def add_confirm_cmd(stderr_path, *keys):
    """Build a no-`--yes` add command that feeds confirm and passphrase on stdin."""
    passphrase_q = shlex.quote(passphrase)
    stderr_path_q = shlex.quote(stderr_path)
    disk_args = " ".join(f"{k}=/dev/disk/by-id/virtio-{k}" for k in keys)
    return (
        f"printf 'yes\\n%s\\n' {passphrase_q} | "
        f"braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 {disk_args} --passphrase-stdin 2> {stderr_path_q}"
    )


# --- Phase 1: Two disks at once → RAID1 from the start ---

with subtest("Two disks create RAID1 pool directly (no balance)"):
    machine.succeed(add_cmd("disk1", "disk2"))

    # Pool is mounted
    machine.succeed("mountpoint -q /mnt/storage")

    # RAID1 profile from the start (mkfs.btrfs -d raid1 -m raid1)
    df_output = machine.succeed("btrfs fi df /mnt/storage")
    assert "Data, RAID1" in df_output, f"Expected RAID1 from start:\n{df_output}"
    assert "Metadata, RAID1" in df_output, f"Expected metadata RAID1:\n{df_output}"

    # Both LUKS mappers exist
    machine.succeed("test -e /dev/mapper/braid-disk1")
    machine.succeed("test -e /dev/mapper/braid-disk2")

    # Both devices in pool
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    for name in ["braid-disk1", "braid-disk2"]:
        assert f"/dev/mapper/{name}" in fi_show, f"{name} missing:\n{fi_show}"

    # Can write data
    machine.succeed("echo 'multi-add data' > /mnt/storage/test.txt")
    machine.succeed("sync")


# --- Phase 2: Add two more disks to existing pool ---

with subtest("Add two disks to existing RAID1 pool"):
    machine.succeed(add_cmd("disk3", "disk4"))

    # All 4 devices in pool
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    for name in ["braid-disk1", "braid-disk2", "braid-disk3", "braid-disk4"]:
        assert f"/dev/mapper/{name}" in fi_show, f"{name} missing:\n{fi_show}"

    # Still RAID1
    df_output = machine.succeed("btrfs fi df /mnt/storage")
    assert "Data, RAID1" in df_output, f"Expected RAID1:\n{df_output}"

with subtest("Data survived multi-add expansion"):
    content = machine.succeed("cat /mnt/storage/test.txt").strip()
    assert content == "multi-add data", f"Expected 'multi-add data', got '{content}'"


# --- Phase 3: Single-disk add to an existing pool ---

with subtest("Single disk add to existing pool works"):
    machine.succeed(add_cmd("disk5"))

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "/dev/mapper/braid-disk5" in fi_show, f"braid-disk5 missing:\n{fi_show}"
    devid_count = fi_show.count("devid")
    assert devid_count == 5, f"Expected 5 devices, got {devid_count}:\n{fi_show}"


# --- Phase 4: Mixed no-op + fresh multi-add ---

with subtest("Mixed already-in-pool and fresh add confirms only fresh disk"):
    err_path = "/tmp/mixed-add.err"
    machine.succeed(add_confirm_cmd(err_path, "disk1", "disk6"))
    err = machine.succeed(f"cat {shlex.quote(err_path)}")

    prompt_start = err.index("Add to pool:")
    prompt_end = err.index("Type 'yes' to continue:")
    confirm_block = err[prompt_start:prompt_end]
    assert "disk6" in confirm_block, f"confirm block should name disk6:\n{confirm_block}"
    assert "disk1" not in confirm_block, f"confirm block should exclude disk1:\n{confirm_block}"

    done_line = next(
        line for line in err.splitlines() if line.startswith("Done. ")
    )
    assert "disk6" in done_line, f"done line should name disk6:\n{done_line}"
    assert "disk1" not in done_line, f"done line should exclude disk1:\n{done_line}"

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "/dev/mapper/braid-disk6" in fi_show, f"braid-disk6 missing:\n{fi_show}"


# --- Phase 5: Idempotent multi-add ---

with subtest("Re-adding already-in-pool disks is a no-op"):
    machine.succeed(add_cmd("disk1", "disk2"))


# --- Phase 6: Duplicate name rejected ---

with subtest("Duplicate name in same command is rejected"):
    machine.fail(add_cmd("disk1", "disk1"))


machine.shutdown()
