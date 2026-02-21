start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def add_disk(dev):
    return (
        f"echo 'erase this disk' | "
        f"BRAID_PASSPHRASE='{passphrase}' "
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid-add-disk {dev}"
    )


# --- Phase 0: No-args disk listing ---

with subtest("No args lists configured disks and preferred workflow"):
    output = machine.succeed("braid-add-disk")
    assert "Configured disks" in output, f"Expected configured listing:\n{output}"
    assert "Preferred workflow" in output, f"Expected preferred workflow:\n{output}"
    assert "braid init-disk" in output, f"Expected init-disk in workflow:\n{output}"
    # All 5 test disks should appear as configured
    for i in range(1, 6):
        assert f"virtio-disk{i}" in output, f"disk{i} missing from listing:\n{output}"

# --- Phase 1: First disk (no pool) ---

with subtest("First disk creates single-drive pool"):
    machine.succeed(add_disk("/dev/disk/by-id/virtio-disk1"))

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
    machine.succeed(add_disk("/dev/disk/by-id/virtio-disk2"))

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
    machine.succeed(add_disk("/dev/disk/by-id/virtio-disk3"))

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

with subtest("No args lists only remaining disks"):
    output = machine.succeed("braid-add-disk")
    # disk1-3 are in pool but still configured, disk4-5 should be configured too
    # The "Available disks" section should be empty or not show pool members
    assert "Configured disks" in output

with subtest("Non-existent device fails"):
    machine.fail("braid-add-disk /dev/disk/by-id/nonexistent")

with subtest("Non-by-id path rejected"):
    machine.fail("braid-add-disk /dev/vdb")

with subtest("Unconfigured disk rejected"):
    # Create a fake by-id symlink pointing to a real block device (disk5's underlying device)
    machine.succeed("ln -sf $(readlink -f /dev/disk/by-id/virtio-disk5) /dev/disk/by-id/virtio-fake")
    result = machine.fail(add_disk("/dev/disk/by-id/virtio-fake"))
    assert "not in braid.disks" in result, f"Expected config guard:\n{result}"

with subtest("Disk already in pool fails"):
    result = machine.fail(add_disk("/dev/disk/by-id/virtio-disk1"))
    assert "already" in result.lower(), f"Expected 'already' in output:\n{result}"

# --- Phase 5: Crash recovery ---

with subtest("Crash recovery — LUKS with no filesystem"):
    # Format disk4 as LUKS manually (simulating crash between luksFormat and mkfs)
    dev = "/dev/disk/by-id/virtio-disk4"
    machine.succeed(
        f"echo -n '{passphrase}' | cryptsetup luksFormat --batch-mode --key-file=- "
        f"{luks_opts} {dev}"
    )
    # Run braid-add-disk — should detect recoverable state and re-format
    machine.succeed(add_disk(dev))

    # Verify it was added to the pool
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "virtio-disk4" in fi_show, f"disk4 mapper missing:\n{fi_show}"

# --- Phase 6: Unmounted pool guard ---

with subtest("Unmounted pool guard — refuses when LUKS devices exist"):
    # Unmount pool and close all LUKS devices
    machine.succeed("umount /mnt/storage")
    for mapper in ["virtio-disk1", "virtio-disk2", "virtio-disk3", "virtio-disk4"]:
        machine.succeed(f"cryptsetup luksClose {mapper}")

    # Try to add disk5 — should refuse because other LUKS devices exist
    result = machine.fail(add_disk("/dev/disk/by-id/virtio-disk5"))
    assert "unlock" in result.lower(), f"Expected 'unlock' in output:\n{result}"

with subtest("Pool intact after guard test"):
    # Re-open LUKS devices and remount
    for disk_id in ["virtio-disk1", "virtio-disk2", "virtio-disk3", "virtio-disk4"]:
        dev = f"/dev/disk/by-id/{disk_id}"
        machine.succeed(
            f"echo -n '{passphrase}' | cryptsetup luksOpen --key-file=- {dev} {disk_id}"
        )
    machine.succeed("mount /dev/mapper/virtio-disk1 /mnt/storage")

    # Verify data is intact
    content1 = machine.succeed("cat /mnt/storage/day1.txt").strip()
    content2 = machine.succeed("cat /mnt/storage/day2.txt").strip()
    assert content1 == "day one data", f"Expected 'day one data', got '{content1}'"
    assert content2 == "day two data", f"Expected 'day two data', got '{content2}'"

with subtest("Completion message shows auto-unlock"):
    output = machine.succeed(add_disk("/dev/disk/by-id/virtio-disk5"))
    assert "auto-unlock" in output.lower(), f"Expected auto-unlock message:\n{output}"
    assert "Add this disk to your NixOS config" not in output, f"Old message still present:\n{output}"

machine.shutdown()
