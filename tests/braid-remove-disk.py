import json

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


def apply_cmd(config=None):
    config_flag = f"--config {config}" if config else ""
    return f"BRAID_PASSPHRASE='{passphrase}' braid apply {config_flag}"


def write_config(disk_list):
    """Write a config file to /tmp simulating nixos-rebuild switch."""
    config = json.dumps({"disks": disk_list, "mountPoint": "/mnt/storage"})
    escaped = config.replace("'", "'\\''")
    return f"echo '{escaped}' > /tmp/braid-config.json"


def remove_disk(dev, phrase="remove this disk"):
    return f"echo '{phrase}' | braid-remove-disk --config /tmp/braid-config.json {dev}"


# --- Phase 0: Build 3-drive RAID1 pool ---

with subtest("Setup: build 3-drive pool with init-disk + apply"):
    machine.succeed(init_disk("/dev/disk/by-id/virtio-disk1"))
    machine.succeed(init_disk("/dev/disk/by-id/virtio-disk2"))
    machine.succeed(init_disk("/dev/disk/by-id/virtio-disk3"))
    machine.succeed(apply_cmd())

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    for name in ["virtio-disk1", "virtio-disk2", "virtio-disk3"]:
        assert f"/dev/mapper/{name}" in fi_show, f"{name} missing:\n{fi_show}"

    machine.succeed("echo 'important data' > /mnt/storage/precious.txt")
    machine.succeed("sync")

# --- Phase 1: Validation errors ---

with subtest("Non-by-id path rejected"):
    machine.succeed(write_config(["/dev/disk/by-id/virtio-disk1", "/dev/disk/by-id/virtio-disk2"]))
    machine.fail(remove_disk("/dev/vdb"))

with subtest("Disk still in config rejected"):
    # disk3 is still in this config
    machine.succeed(write_config([
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk2",
        "/dev/disk/by-id/virtio-disk3",
    ]))
    result = machine.fail(remove_disk("/dev/disk/by-id/virtio-disk3"))
    assert "still in" in result.lower(), f"Expected config guard:\n{result}"

# --- Phase 2: Graceful remove (Tier 1) ---

with subtest("Graceful remove of disk3"):
    machine.succeed(write_config([
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk2",
    ]))
    machine.succeed(remove_disk("/dev/disk/by-id/virtio-disk3"))

with subtest("disk3 gone from pool, disk1+disk2 remain, RAID1 profile"):
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print(f"Pool after graceful remove:\n{fi_show}")
    assert "virtio-disk3" not in fi_show, f"disk3 still in pool:\n{fi_show}"
    for name in ["virtio-disk1", "virtio-disk2"]:
        assert f"/dev/mapper/{name}" in fi_show, f"{name} missing:\n{fi_show}"

    df_output = machine.succeed("btrfs fi df /mnt/storage")
    assert "RAID1" in df_output, f"Expected RAID1 profile:\n{df_output}"

with subtest("LUKS mapper closed after graceful remove"):
    machine.fail("test -e /dev/mapper/virtio-disk3")

with subtest("Data intact after graceful remove"):
    content = machine.succeed("cat /mnt/storage/precious.txt").strip()
    assert content == "important data", f"Expected 'important data', got '{content}'"

# --- Phase 3: No-args listing ---

with subtest("No args shows removable disks"):
    machine.succeed(write_config(["/dev/disk/by-id/virtio-disk1"]))
    output = machine.succeed("braid-remove-disk --config /tmp/braid-config.json")
    assert "removable" in output.lower() or "Removable" in output, (
        f"Expected removable listing:\n{output}"
    )

# --- Phase 4: Redundancy warning ---

with subtest("Redundancy warning blocks normal confirmation"):
    machine.succeed(write_config(["/dev/disk/by-id/virtio-disk1"]))
    machine.fail(remove_disk("/dev/disk/by-id/virtio-disk2", "remove this disk"))

with subtest("Redundancy warning accepts escalated confirmation"):
    machine.succeed(remove_disk(
        "/dev/disk/by-id/virtio-disk2",
        "remove this disk without redundancy",
    ))

with subtest("Pool has 1 device after redundancy removal"):
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print(f"Pool after redundancy removal:\n{fi_show}")
    devid_count = fi_show.count("devid")
    assert devid_count == 1, f"Expected 1 device, got {devid_count}:\n{fi_show}"
    assert "virtio-disk1" in fi_show, f"disk1 missing:\n{fi_show}"

with subtest("Data intact after redundancy removal"):
    content = machine.succeed("cat /mnt/storage/precious.txt").strip()
    assert content == "important data", f"Expected 'important data', got '{content}'"

# --- Phase 5: Rebuild pool for Tier 2 test ---

with subtest("Rebuild pool: re-add disk2 and disk3"):
    # Sequential: add one disk at a time to avoid ENOSPC during balance
    machine.succeed(init_disk("/dev/disk/by-id/virtio-disk2", force=True))
    machine.succeed(write_config([
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk2",
    ]))
    machine.succeed(apply_cmd(config="/tmp/braid-config.json"))

    machine.succeed(init_disk("/dev/disk/by-id/virtio-disk3", force=True))
    machine.succeed(write_config([
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk2",
        "/dev/disk/by-id/virtio-disk3",
    ]))
    machine.succeed(apply_cmd(config="/tmp/braid-config.json"))

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    for name in ["virtio-disk1", "virtio-disk2", "virtio-disk3"]:
        assert f"/dev/mapper/{name}" in fi_show, f"{name} missing:\n{fi_show}"

# --- Phase 6: Remove-missing (Tier 2) ---

with subtest("Simulate disk3 death and mount degraded"):
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup luksClose virtio-disk3")
    machine.succeed("mount -o degraded /dev/mapper/virtio-disk1 /mnt/storage")

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print(f"Pool after simulated death:\n{fi_show}")
    assert "missing" in fi_show.lower(), f"Expected missing device:\n{fi_show}"

with subtest("Remove-missing succeeds for dead disk3"):
    machine.succeed(write_config([
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk2",
    ]))
    machine.succeed(remove_disk("/dev/disk/by-id/virtio-disk3"))

with subtest("No missing devices after remove-missing"):
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print(f"Pool after remove-missing:\n{fi_show}")
    assert "missing" not in fi_show.lower(), f"Still has missing device:\n{fi_show}"

with subtest("Data intact after remove-missing"):
    content = machine.succeed("cat /mnt/storage/precious.txt").strip()
    assert content == "important data", f"Expected 'important data', got '{content}'"

# --- Phase 7: Tier 3 — fail with diagnostic ---

with subtest("Tier 3: disk not open and not missing gives clear error"):
    # disk3 is now closed and not in the pool — neither open nor missing
    machine.succeed(write_config([
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk2",
    ]))
    result = machine.fail(remove_disk("/dev/disk/by-id/virtio-disk3"))
    print(f"Tier 3 error output:\n{result}")

machine.shutdown()
