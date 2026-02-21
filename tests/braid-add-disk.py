start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def init_disk(dev, force=False):
    force_flag = "--force" if force else ""
    confirm = "BRAID_CONFIRM='reformat this disk' " if force else ""
    return (
        f"{confirm}"
        f"BRAID_PASSPHRASE='{passphrase}' "
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid init-disk {force_flag} {dev}"
    )


def apply_cmd(config=None, extra="", confirm=""):
    config_flag = f"--config {config}" if config else ""
    env = f"BRAID_PASSPHRASE='{passphrase}'"
    if confirm:
        env += f" BRAID_CONFIRM='{confirm}'"
    return f"{env} braid apply {config_flag} {extra}"


# --- Phase 1: First disk (no pool) ---

with subtest("First disk creates single-drive pool"):
    machine.succeed(init_disk("/dev/disk/by-id/virtio-disk1"))
    machine.succeed(apply_cmd())

    # Pool is mounted
    machine.succeed("mountpoint -q /mnt/storage")

    # Single profile (only 1 drive)
    df_output = machine.succeed("btrfs fi df /mnt/storage")
    assert "Data, single" in df_output, f"Expected single profile:\n{df_output}"

    # LUKS mapper exists with correct name (by-id basename)
    machine.succeed("test -e /dev/mapper/virtio-disk1")

    # Can write data
    machine.succeed("echo 'day one data' > /mnt/storage/day1.txt")
    machine.succeed("sync")

# --- Phase 2: Second disk (convert to RAID1) ---

with subtest("Second disk converts pool to RAID1"):
    machine.succeed(init_disk("/dev/disk/by-id/virtio-disk2"))
    machine.succeed(apply_cmd())

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
    machine.succeed(init_disk("/dev/disk/by-id/virtio-disk3"))
    machine.succeed(apply_cmd())

    # All 3 mapper devices in pool
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    for name in ["virtio-disk1", "virtio-disk2", "virtio-disk3"]:
        assert f"/dev/mapper/{name}" in fi_show, f"{name} missing:\n{fi_show}"

with subtest("All data survived third disk addition"):
    content1 = machine.succeed("cat /mnt/storage/day1.txt").strip()
    content2 = machine.succeed("cat /mnt/storage/day2.txt").strip()
    assert content1 == "day one data", f"Expected 'day one data', got '{content1}'"
    assert content2 == "day two data", f"Expected 'day two data', got '{content2}'"

# --- Phase 4: Validation errors ---

with subtest("Non-existent device fails init-disk"):
    machine.fail(init_disk("/dev/disk/by-id/nonexistent"))

with subtest("Non-by-id path rejected"):
    machine.fail(init_disk("/dev/vdb"))

with subtest("Unconfigured disk rejected"):
    # Create a fake by-id symlink pointing to a real block device (disk5's underlying device)
    machine.succeed("ln -sf $(readlink -f /dev/disk/by-id/virtio-disk5) /dev/disk/by-id/virtio-fake")
    result = machine.fail(init_disk("/dev/disk/by-id/virtio-fake") + " 2>&1")
    assert "not declared" in result.lower() or "braid.disks" in result, f"Expected config guard:\n{result}"

with subtest("Disk already in pool fails init-disk"):
    result = machine.fail(init_disk("/dev/disk/by-id/virtio-disk1") + " 2>&1")
    assert "currently part of" in result.lower() or "already" in result.lower(), (
        f"Expected pool membership guard:\n{result}"
    )

# --- Phase 5: Crash recovery ---

with subtest("Crash recovery — LUKS with no filesystem"):
    # Format disk4 as LUKS manually (simulating crash between luksFormat and mkfs)
    dev = "/dev/disk/by-id/virtio-disk4"
    machine.succeed(
        f"echo -n '{passphrase}' | cryptsetup luksFormat --batch-mode --key-file=- "
        f"{luks_opts} {dev}"
    )
    # Run init-disk with --force (already LUKS), then apply
    machine.succeed(init_disk(dev, force=True))
    machine.succeed(apply_cmd())

    # Verify it was added to the pool
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "virtio-disk4" in fi_show, f"disk4 mapper missing:\n{fi_show}"

# --- Phase 6: Fifth disk expands pool ---

with subtest("Fifth disk expands pool"):
    machine.succeed(init_disk("/dev/disk/by-id/virtio-disk5"))
    machine.succeed(apply_cmd())

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "virtio-disk5" in fi_show, f"disk5 missing:\n{fi_show}"
    devid_count = fi_show.count("devid")
    assert devid_count == 5, f"Expected 5 devices, got {devid_count}:\n{fi_show}"

with subtest("All data survived fifth disk addition"):
    content1 = machine.succeed("cat /mnt/storage/day1.txt").strip()
    content2 = machine.succeed("cat /mnt/storage/day2.txt").strip()
    assert content1 == "day one data", f"Expected 'day one data', got '{content1}'"
    assert content2 == "day two data", f"Expected 'day two data', got '{content2}'"

machine.shutdown()
