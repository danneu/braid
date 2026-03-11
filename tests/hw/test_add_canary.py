#!/usr/bin/env python3
"""
# Test: hardware canary — braid add lifecycle
#
# Intent: Revalidate the `braid add` lifecycle (first disk creates pool,
# second converts to RAID1, third expands) on real hardware with real
# /dev/disk/by-id paths, real LUKS timing, and real btrfs balance.
#
# Why it exists: VM tests use 256-1024 MiB virtual disks where LUKS format
# and btrfs balance are near-instant. Real 500 GB drives exercise actual
# timing, udev symlink resolution, and physical I/O patterns.
#
# Scenario: User adds three physical drives one at a time, building up a
# RAID1 pool. Mirrors the exact flow from the VM test.
#
# Revalidates VM test: tests/cli/braid-add-disk.py (phases 1-3)
"""

import sys
import os

sys.path.insert(0, os.path.dirname(__file__))
from harness import (
    run, run_capture, cleanup, section,
    add_cmd, disk, disk_name, MOUNT_POINT,
)

# --- Phase 1: First disk (no pool) ---

with section("Phase 1: first disk creates single-drive pool"):
    run(add_cmd("hwtest1"))

    # Pool is mounted
    run(f"mountpoint -q {MOUNT_POINT}")

    # Single profile (only 1 drive)
    df_output = run(f"btrfs fi df {MOUNT_POINT}")
    assert "Data, single" in df_output, f"Expected single profile:\n{df_output}"
    assert "Metadata, DUP" in df_output, f"Expected DUP metadata:\n{df_output}"

    # LUKS mapper exists
    run("test -e /dev/mapper/braid-hwtest1")

    # Can write data
    run(f"echo 'day one data' > {MOUNT_POINT}/day1.txt")
    run("sync")

# --- Phase 2: Second disk (convert to RAID1) ---

with section("Phase 2: second disk converts pool to RAID1"):
    run(add_cmd("hwtest2"))

    df_output = run(f"btrfs fi df {MOUNT_POINT}")
    assert "Data, RAID1" in df_output, f"Expected RAID1:\n{df_output}"

with section("Phase 2: day 1 data survived RAID1 conversion"):
    content = run(f"cat {MOUNT_POINT}/day1.txt").strip()
    assert content == "day one data", f"Expected 'day one data', got '{content}'"

with section("Phase 2: write more data on RAID1"):
    run(f"echo 'day two data' > {MOUNT_POINT}/day2.txt")
    run("sync")

# --- Phase 3: Third disk (add to RAID1) ---

with section("Phase 3: third disk expands RAID1 pool"):
    run(add_cmd("hwtest3"))

    # All 3 mapper devices in pool
    fi_show = run(f"btrfs fi show {MOUNT_POINT}")
    for name in ["braid-hwtest1", "braid-hwtest2", "braid-hwtest3"]:
        assert f"/dev/mapper/{name}" in fi_show, f"{name} missing:\n{fi_show}"

    devid_count = fi_show.count("devid")
    assert devid_count == 3, f"Expected 3 devices, got {devid_count}:\n{fi_show}"

with section("Phase 3: all data survived third disk addition"):
    content1 = run(f"cat {MOUNT_POINT}/day1.txt").strip()
    content2 = run(f"cat {MOUNT_POINT}/day2.txt").strip()
    assert content1 == "day one data", f"Expected 'day one data', got '{content1}'"
    assert content2 == "day two data", f"Expected 'day two data', got '{content2}'"

print("\nAll add canary phases passed.")
