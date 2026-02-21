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


def apply_cmd():
    return f"BRAID_PASSPHRASE='{passphrase}' braid apply"


# --- Phase 1: Single-disk pool ---

with subtest("Setup — add disk1 only"):
    machine.succeed(init_disk("/dev/disk/by-id/virtio-disk1"))
    machine.succeed(apply_cmd())
    machine.succeed("mountpoint -q /mnt/storage")

with subtest("Single-disk summary"):
    output = machine.succeed("braid status")
    print(f"Single-disk status:\n{output}")
    assert "healthy" in output, f"Expected 'healthy':\n{output}"
    assert "Drives:   1" in output, f"Expected 'Drives:   1':\n{output}"
    assert "single" in output, f"Expected 'single' profile:\n{output}"
    assert "Total:" in output, f"Expected 'Total:':\n{output}"
    assert "Used:" in output, f"Expected 'Used:':\n{output}"
    assert "Free:" in output, f"Expected 'Free:':\n{output}"
    assert "RAID1" not in output, f"Unexpected 'RAID1' in single-disk:\n{output}"
    assert "missing" not in output.lower(), f"Unexpected 'missing':\n{output}"

# --- Phase 2: RAID1 pool ---

with subtest("Setup — add disk2 and disk3 for RAID1"):
    machine.succeed(init_disk("/dev/disk/by-id/virtio-disk2"))
    machine.succeed(init_disk("/dev/disk/by-id/virtio-disk3"))
    machine.succeed(apply_cmd())
    df_output = machine.succeed("btrfs fi df /mnt/storage")
    assert "RAID1" in df_output, f"Expected RAID1 after adding 3 disks:\n{df_output}"

with subtest("Healthy RAID1 summary"):
    output = machine.succeed("braid status")
    print(f"Healthy RAID1 status:\n{output}")
    assert "healthy" in output, f"Expected 'healthy':\n{output}"
    assert "Drives:   3" in output, f"Expected 'Drives:   3':\n{output}"
    assert "RAID1" in output, f"Expected 'RAID1':\n{output}"
    assert "Total:" in output, f"Expected 'Total:':\n{output}"
    assert "Used:" in output, f"Expected 'Used:':\n{output}"
    assert "Free:" in output, f"Expected 'Free:':\n{output}"
    assert "scrub" in output.lower(), f"Expected 'scrub':\n{output}"
    assert "missing" not in output.lower(), f"Unexpected 'missing':\n{output}"

with subtest("Healthy verbose"):
    output = machine.succeed("braid status --verbose")
    print(f"Healthy verbose:\n{output}")
    lines = output.splitlines()
    for disk in ["virtio-disk1", "virtio-disk2", "virtio-disk3"]:
        disk_lines = [l for l in lines if disk in l and "present" in l]
        assert disk_lines, f"{disk} not shown as present:\n{output}"
    assert "devid" in output, f"Expected 'devid':\n{output}"
    assert "LUKS:" in output, f"Expected 'LUKS:':\n{output}"
    assert "Errors:" in output, f"Expected 'Errors:':\n{output}"

# --- Phase 3: Degraded pool ---

with subtest("Simulate drive failure — close disk3"):
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup luksClose virtio-disk3")
    machine.succeed("mount -o degraded /dev/mapper/virtio-disk1 /mnt/storage")
    machine.succeed("mountpoint -q /mnt/storage")

with subtest("Degraded summary"):
    output = machine.succeed("braid status")
    print(f"Degraded status:\n{output}")
    assert "DEGRADED" in output, f"Expected 'DEGRADED':\n{output}"
    assert "missing" in output.lower(), f"Expected 'missing':\n{output}"
    assert "RAID1" in output, f"Expected 'RAID1':\n{output}"
    assert "2 present, 1 missing" in output, f"Expected '2 present, 1 missing':\n{output}"

with subtest("Degraded verbose"):
    output = machine.succeed("braid status --verbose")
    print(f"Degraded verbose:\n{output}")
    lines = output.splitlines()
    assert "MISSING" in output, f"Expected 'MISSING':\n{output}"
    assert "virtio-disk3" in output, f"Expected 'virtio-disk3':\n{output}"
    assert "not found" in output or "device absent" in output, (
        f"Expected 'not found' or 'device absent':\n{output}"
    )
    for disk in ["virtio-disk1", "virtio-disk2"]:
        disk_lines = [l for l in lines if disk in l and "present" in l]
        assert disk_lines, f"{disk} not shown as present:\n{output}"

# --- Phase 4: Error cases ---

with subtest("Error on unmounted pool"):
    machine.succeed("umount /mnt/storage")
    result = machine.fail("braid status 2>&1")
    assert "not mounted" in result.lower(), f"Expected 'not mounted':\n{result}"

machine.shutdown()
