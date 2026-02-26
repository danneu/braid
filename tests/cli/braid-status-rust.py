# Test: braid status (Rust implementation) across pool states
#
# What: Validates the Rust braid status subcommand end-to-end against real
# virtual disks — single-disk, RAID1, degraded, and not-mounted states in both
# human and JSON output modes (including --verbose).
#
# Why: The Rust CLI must produce correct status reports from real disk state.
# This test bridges unit tests (pure logic) with integration: real LUKS, real
# btrfs, real command output parsed by the Rust probe and status layers.
#
# Dependencies: `braid add` must correctly format LUKS, create/extend btrfs pool,
# and mount at the configured mount point. Rust braid binary for status.

import json

start_all()
machine.wait_for_unit("multi-user.target")

import shlex

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def add_disk(key):
    passphrase_q = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {passphrase_q} | "
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid add {key} --passphrase-stdin --yes"
    )


def rust_status(extra=""):
    return f"braid status {extra}"


# --- Phase 1: Single-disk summary ---

with subtest("Setup: single disk pool"):
    machine.succeed(add_disk("disk1"))
    machine.succeed("mountpoint -q /mnt/storage")

with subtest("Single-disk summary"):
    output = machine.succeed(rust_status())
    print(f"Single-disk status:\n{output}")
    assert "healthy" in output, f"Expected 'healthy':\n{output}"
    assert "Drives:   1" in output, f"Expected 'Drives:   1':\n{output}"
    assert "single" in output, f"Expected 'single' profile:\n{output}"
    assert "Total:" in output, f"Expected 'Total:':\n{output}"
    assert "Used:" in output, f"Expected 'Used:':\n{output}"
    assert "Free:" in output, f"Expected 'Free:':\n{output}"
    assert "RAID1" not in output, f"Unexpected 'RAID1' in single-disk:\n{output}"
    assert "missing" not in output.lower(), f"Unexpected 'missing':\n{output}"

# --- Phase 2: RAID1 healthy ---

with subtest("Setup: 3-disk RAID1 pool"):
    machine.succeed(add_disk("disk2"))
    machine.succeed(add_disk("disk3"))
    df_output = machine.succeed("btrfs fi df /mnt/storage")
    assert "RAID1" in df_output, f"Expected RAID1 after adding 3 disks:\n{df_output}"

with subtest("Healthy RAID1 summary"):
    output = machine.succeed(rust_status())
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
    output = machine.succeed(rust_status("--verbose"))
    print(f"Healthy verbose:\n{output}")
    lines = output.splitlines()
    for disk in ["disk1", "disk2", "disk3"]:
        disk_lines = [l for l in lines if disk in l and "present" in l]
        assert disk_lines, f"{disk} not shown as present:\n{output}"
    assert "devid" in output, f"Expected 'devid':\n{output}"
    assert "LUKS:" in output, f"Expected 'LUKS:':\n{output}"
    assert "Errors:" in output, f"Expected 'Errors:':\n{output}"

with subtest("Healthy JSON verbose"):
    raw = machine.succeed(rust_status("--json --verbose"))
    s = json.loads(raw)
    assert s["status_code"] == "healthy", f"Expected healthy: {s['status_code']}"
    assert len(s["disks"]) == 3, f"Expected 3 disks: {len(s['disks'])}"
    for d in s["disks"]:
        assert "mapper" in d, f"Missing mapper: {d}"
        assert "by_id" in d, f"Missing by_id: {d}"
        assert "luks_uuid" in d, f"Missing luks_uuid: {d}"
        assert "devid" in d, f"Missing devid: {d}"
        assert d["status"] == "present", f"Expected present: {d}"
        assert "errors" in d, f"Missing errors: {d}"
        assert d["errors"] is not None, f"Expected errors object: {d}"
        for key in ["read", "write", "corruption"]:
            assert key in d["errors"], f"Missing errors.{key}: {d}"

# --- Phase 3: Degraded ---

with subtest("Simulate drive failure - close disk3"):
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup close braid-disk3")
    machine.succeed("mount -o degraded /dev/mapper/braid-disk1 /mnt/storage")
    machine.succeed("mountpoint -q /mnt/storage")

with subtest("Degraded summary"):
    output = machine.succeed(rust_status())
    print(f"Degraded status:\n{output}")
    assert "DEGRADED" in output, f"Expected 'DEGRADED':\n{output}"
    assert "missing" in output.lower(), f"Expected 'missing':\n{output}"
    assert "RAID1" in output, f"Expected 'RAID1':\n{output}"
    assert "2 present, 1 missing" in output, f"Expected '2 present, 1 missing':\n{output}"

with subtest("Degraded verbose"):
    output = machine.succeed(rust_status("--verbose"))
    print(f"Degraded verbose:\n{output}")
    assert "MISSING" in output, f"Expected 'MISSING':\n{output}"
    assert "disk3" in output, f"Expected 'disk3':\n{output}"
    assert "not found" in output or "device absent" in output, (
        f"Expected 'not found' or 'device absent':\n{output}"
    )
    lines = output.splitlines()
    for disk in ["disk1", "disk2"]:
        disk_lines = [l for l in lines if disk in l and "present" in l]
        assert disk_lines, f"{disk} not shown as present:\n{output}"

with subtest("Degraded JSON verbose"):
    raw = machine.succeed(rust_status("--json --verbose"))
    s = json.loads(raw)
    assert s["status_code"] == "degraded", f"Expected degraded: {s['status_code']}"
    present_disks = [d for d in s["disks"] if d["status"] == "present"]
    missing_disks = [d for d in s["disks"] if d["status"] == "missing"]
    assert len(present_disks) >= 2, f"Expected at least 2 present disks: {present_disks}"
    assert len(missing_disks) >= 1, f"Expected at least 1 missing disk: {missing_disks}"

# --- Phase 4: Not mounted ---

with subtest("Not mounted"):
    machine.succeed("umount /mnt/storage")
    output = machine.succeed(rust_status())
    print(f"Not mounted status:\n{output}")
    assert "not mounted" in output.lower(), f"Expected 'not mounted':\n{output}"

with subtest("Not mounted JSON"):
    raw = machine.succeed(rust_status("--json"))
    s = json.loads(raw)
    assert s["status_code"] == "not_mounted", f"Expected not_mounted: {s['status_code']}"
    assert s["status"] == "not mounted", f"Expected 'not mounted': {s['status']}"
    assert s["disks"] == [], f"Expected empty disks: {s['disks']}"
    assert "capacity" not in s, f"Unexpected capacity: {s}"
    assert "profile" not in s, f"Unexpected profile: {s}"
    assert "total_devices" not in s, f"Unexpected total_devices: {s}"

machine.shutdown()
