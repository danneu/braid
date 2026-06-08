# Test: braid unified CLI — status reporting and error cases
#
# What: Exercises `braid status` in all output modes (human, --json) against a
# 3-disk RAID1 pool, and validates the "not mounted" error case.
#
# Why: The unified CLI must produce correct, complete status reports after pool
# setup using intent commands (`braid add`). This covers the primary read path
# that operators use to check pool health.
#
# Dependencies: `braid add` must correctly format LUKS, create/extend btrfs pool,
# and mount at the configured mount point. `braid status` must read live pool state.

import json
import re

start_all()
machine.wait_for_unit("multi-user.target")

import shlex

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"
UUID_RE = re.compile(r"[0-9a-f-]{36}")


def add_disk(key):
    passphrase_q = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {passphrase_q} | "
        f"braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 {key}=/dev/disk/by-id/virtio-{key} --passphrase-stdin --yes"
    )


# --- Phase 0: Build 3-disk RAID1 pool ---

with subtest("Setup: build 3-disk RAID1 pool"):
    machine.succeed(add_disk("disk1"))
    machine.succeed(add_disk("disk2"))
    machine.succeed(add_disk("disk3"))
    machine.succeed("echo 'test data' > /mnt/storage/file.txt && sync")

    fi_df = machine.succeed("btrfs fi df /mnt/storage")
    assert "RAID1" in fi_df, f"Expected RAID1:\n{fi_df}"

# --- Phase 1: braid status (human output) ---

with subtest("braid status shows pool summary with per-disk detail"):
    output = machine.succeed("braid status")
    print(f"braid status output:\n{output}")
    assert "intact" in output, f"Expected 'intact':\n{output}"
    assert "Drives:" in output, f"Expected 'Drives:':\n{output}"
    for disk in ["disk1", "disk2", "disk3"]:
        assert disk in output, f"Expected '{disk}':\n{output}"
    assert "RAID1" in output, f"Expected 'RAID1':\n{output}"
    assert "Profile:" in output, f"Expected 'Profile:' header:\n{output}"
    assert "Data:      RAID1" in output, f"Expected 'Data:      RAID1':\n{output}"
    assert "Metadata:  RAID1" in output, f"Expected 'Metadata:  RAID1':\n{output}"
    assert "System:    RAID1" in output, f"Expected 'System:    RAID1':\n{output}"
    assert "no redundancy" not in output, (
        f"3-disk RAID1 pool must not report 'no redundancy':\n{output}"
    )
    assert "Total:" in output, f"Expected 'Total:':\n{output}"
    assert "Used:" in output, f"Expected 'Used:':\n{output}"
    assert "Free:" in output, f"Expected 'Free:':\n{output}"
    assert "scrub" in output.lower(), f"Expected 'scrub':\n{output}"
    # Per-disk detail (always shown)
    lines = output.splitlines()
    fsid_lines = [l for l in lines if l.startswith("FSID:")]
    assert fsid_lines, f"Expected FSID line:\n{output}"
    assert UUID_RE.search(fsid_lines[0]), f"Expected UUID in FSID line:\n{output}"
    for disk in ["disk1", "disk2", "disk3"]:
        disk_lines = [l for l in lines if disk in l and "present" in l]
        assert disk_lines, f"{disk} not shown as present:\n{output}"
    assert "devid" in output, f"Expected 'devid':\n{output}"
    assert "LUKS:" in output, f"Expected 'LUKS:':\n{output}"
    assert "btrfs:" in output, f"Expected 'btrfs:':\n{output}"
    assert "SMART:" in output, f"Expected 'SMART:':\n{output}"

# --- Phase 2: braid status --json ---

with subtest("braid status --json has schema fields and disk details"):
    raw = machine.succeed("braid status --json")
    s = json.loads(raw)
    assert s["mount_point"] == "/mnt/storage", f"Bad mount_point: {s['mount_point']}"
    assert s["status"] == "intact", f"Bad status: {s['status']}"
    assert s["total_devices"] == 3, f"Bad total_devices: {s['total_devices']}"
    assert s["present_count"] == 3, f"Bad present_count: {s['present_count']}"
    assert s["missing_count"] == 0, f"Bad missing_count: {s['missing_count']}"
    assert "missing_devids" not in s, f"missing_devids should be omitted when empty: {s}"
    assert s["profile"] == {
        "data": ["RAID1"],
        "metadata": ["RAID1"],
        "system": ["RAID1"],
    }, f"Bad profile: {s['profile']!r}"
    assert "fsid" in s, f"Missing fsid: {s}"
    assert UUID_RE.fullmatch(s["fsid"]), f"Bad fsid: {s['fsid']}"
    assert "total_bytes" in s["capacity"], "Missing capacity.total_bytes"
    assert "used_bytes" in s["capacity"], "Missing capacity.used_bytes"
    assert "free_bytes" in s["capacity"], "Missing capacity.free_bytes"
    assert s["capacity"]["total_bytes"] > 0, "total_bytes should be positive"
    assert isinstance(s["last_scrub"], dict), "last_scrub should be an object"
    assert "state" in s["last_scrub"], "last_scrub should have a state field"
    assert len(s["disks"]) == 3, f"Expected 3 disks: {s['disks']}"
    for disk in s["disks"]:
        assert "mapper" in disk, f"Disk missing mapper: {disk}"
        assert "devid" in disk, f"Disk missing devid: {disk}"
        assert "status" in disk, f"Disk missing status: {disk}"
        assert disk["status"] == "present", f"Disk not present: {disk}"
        assert "btrfs_errors" in disk, f"Disk missing btrfs_errors: {disk}"
        assert "smart" in disk, f"Disk missing smart: {disk}"

# --- Phase 3: Error cases ---

with subtest("braid status reports not mounted on unmounted pool"):
    machine.succeed("umount /mnt/storage")
    output = machine.succeed("braid status")
    assert "not mounted" in output.lower(), f"Expected 'not mounted' in output:\n{output}"

    json_output = machine.succeed("braid status --json")
    s = json.loads(json_output)
    assert s["status"] == "not_mounted", f"Expected status 'not_mounted':\n{s}"
    assert "mount_point" in s, f"Expected mount_point in JSON:\n{s}"

machine.shutdown()
