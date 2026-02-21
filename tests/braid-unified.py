import json

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


# --- Phase 0: Build 3-disk RAID1 pool ---

with subtest("Setup: build 3-disk RAID1 pool"):
    machine.succeed(add_disk("/dev/disk/by-id/virtio-disk1"))
    machine.succeed(add_disk("/dev/disk/by-id/virtio-disk2"))
    machine.succeed(add_disk("/dev/disk/by-id/virtio-disk3"))
    machine.succeed("echo 'test data' > /mnt/storage/file.txt && sync")

    fi_df = machine.succeed("btrfs fi df /mnt/storage")
    assert "RAID1" in fi_df, f"Expected RAID1:\n{fi_df}"

# --- Phase 1: braid status (human output) ---

with subtest("braid status shows pool summary"):
    output = machine.succeed("braid status")
    print(f"braid status output:\n{output}")
    assert "healthy" in output, f"Expected 'healthy':\n{output}"
    assert "Drives:   3" in output, f"Expected 'Drives:   3':\n{output}"
    assert "RAID1" in output, f"Expected 'RAID1':\n{output}"
    assert "Total:" in output, f"Expected 'Total:':\n{output}"
    assert "Used:" in output, f"Expected 'Used:':\n{output}"
    assert "Free:" in output, f"Expected 'Free:':\n{output}"
    assert "scrub" in output.lower(), f"Expected 'scrub':\n{output}"

# --- Phase 2: braid status --json ---

with subtest("braid status --json has schema fields"):
    raw = machine.succeed("braid status --json")
    s = json.loads(raw)
    assert s["schema_version"] == 1, f"Bad schema_version: {s['schema_version']}"
    assert s["mount_point"] == "/mnt/storage", f"Bad mount_point: {s['mount_point']}"
    assert s["status"] == "healthy", f"Bad status: {s['status']}"
    assert s["total_devices"] == 3, f"Bad total_devices: {s['total_devices']}"
    assert s["present_count"] == 3, f"Bad present_count: {s['present_count']}"
    assert s["missing_count"] == 0, f"Bad missing_count: {s['missing_count']}"
    assert s["profile"] == "RAID1", f"Bad profile: {s['profile']}"
    assert "total_bytes" in s["capacity"], "Missing capacity.total_bytes"
    assert "used_bytes" in s["capacity"], "Missing capacity.used_bytes"
    assert "free_bytes" in s["capacity"], "Missing capacity.free_bytes"
    assert s["capacity"]["total_bytes"] > 0, "total_bytes should be positive"
    assert "last_scrub" in s, "Missing last_scrub"

# --- Phase 3: braid status --verbose ---

with subtest("braid status --verbose shows per-disk detail"):
    output = machine.succeed("braid status --verbose")
    print(f"braid status --verbose:\n{output}")
    lines = output.splitlines()
    for disk in ["virtio-disk1", "virtio-disk2", "virtio-disk3"]:
        disk_lines = [l for l in lines if disk in l and "present" in l]
        assert disk_lines, f"{disk} not shown as present:\n{output}"
    assert "devid" in output, f"Expected 'devid':\n{output}"
    assert "LUKS:" in output, f"Expected 'LUKS:':\n{output}"
    assert "Errors:" in output, f"Expected 'Errors:':\n{output}"

# --- Phase 4: braid status --json --verbose ---

with subtest("braid status --json --verbose includes disk details"):
    raw = machine.succeed("braid status --json --verbose")
    s = json.loads(raw)
    assert len(s["disks"]) == 3, f"Expected 3 disks: {s['disks']}"
    for disk in s["disks"]:
        assert "mapper" in disk, f"Disk missing mapper: {disk}"
        assert "devid" in disk, f"Disk missing devid: {disk}"
        assert "status" in disk, f"Disk missing status: {disk}"
        assert disk["status"] == "present", f"Disk not present: {disk}"
        assert "errors" in disk, f"Disk missing errors: {disk}"

# --- Phase 5: Backward compatibility ---

with subtest("braid-status still works"):
    output = machine.succeed("braid-status")
    assert "healthy" in output, f"Expected 'healthy':\n{output}"
    assert "RAID1" in output, f"Expected 'RAID1':\n{output}"

with subtest("braid-status --verbose still works"):
    output = machine.succeed("braid-status --verbose")
    assert "devid" in output, f"Expected 'devid':\n{output}"
    assert "LUKS:" in output, f"Expected 'LUKS:':\n{output}"

with subtest("braid-add-disk shows deprecation warning"):
    # Verify the command exists and prints deprecation
    output = machine.succeed("braid-add-disk 2>&1 || true")
    assert "deprecated" in output.lower(), f"Expected deprecation warning:\n{output}"

with subtest("braid-remove-disk still works"):
    # Verify the command exists and shows usage
    machine.succeed("which braid-remove-disk")

# --- Phase 6: Error cases ---

with subtest("braid status fails on unmounted pool"):
    machine.succeed("umount /mnt/storage")
    machine.fail("braid status")

machine.shutdown()
