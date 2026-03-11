#!/usr/bin/env python3
"""
# Test: hardware canary — braid replace (live disk)
#
# Intent: Revalidate `braid replace --old disk2 --new disk3` on real
# hardware: the full add→balance→remove→close pipeline with real I/O
# timing and 500 GB block device sizes.
#
# Why it exists: VM tests replace on tiny virtual disks where balance
# completes in seconds. Real 500 GB drives exercise actual btrfs balance
# timing, real LUKS open/close sequencing, and physical I/O patterns.
#
# Scenario: Operator swaps a slow-but-alive drive for a faster one
# without downtime. The pool stays healthy throughout.
#
# Revalidates VM test: tests/cli/replace-live-disk.py (phase 1)
"""

import json
import sys
import os

sys.path.insert(0, os.path.dirname(__file__))
from harness import (
    run, run_capture, cleanup, section,
    add_cmd, replace_cmd, CONFIG,
)

# --- Phase 0: Build 2-drive RAID1 pool ---

with section("Setup: build 2-drive RAID1 pool"):
    run(add_cmd("disk1"))
    run(add_cmd("disk2"))

    fi_show = run("btrfs fi show /mnt/storage")
    for name in ["braid-disk1", "braid-disk2"]:
        assert f"/dev/mapper/{name}" in fi_show, f"{name} missing:\n{fi_show}"

    df_output = run("btrfs fi df /mnt/storage")
    assert "RAID1" in df_output, f"Expected RAID1 profile:\n{df_output}"

    run("echo 'important data' > /mnt/storage/precious.txt")
    run("sync")

# --- Phase 1: Live replace disk2 → disk3 ---

with section("Live replace disk2 with disk3"):
    result = run(replace_cmd("disk2", "disk3"), timeout=1800)
    print(f"braid replace output:\n{result}")

with section("Pool healthy after live replace"):
    fi_show = run("btrfs fi show /mnt/storage")
    print(f"Pool after live replace:\n{fi_show}")

    assert "/dev/mapper/braid-disk3" in fi_show, \
        f"New disk braid-disk3 missing from pool:\n{fi_show}"
    assert "braid-disk2" not in fi_show, \
        f"Old disk braid-disk2 should be removed:\n{fi_show}"
    assert "missing" not in fi_show.lower(), \
        f"Pool should have no missing devices:\n{fi_show}"
    assert "/dev/mapper/braid-disk1" in fi_show, \
        f"braid-disk1 missing from pool:\n{fi_show}"

    devid_count = fi_show.count("devid")
    assert devid_count == 2, f"Expected 2 devices, got {devid_count}:\n{fi_show}"

    df_output = run("btrfs fi df /mnt/storage")
    assert "RAID1" in df_output, f"Expected RAID1 profile:\n{df_output}"

with section("Old disk LUKS mapper closed"):
    exitcode, _ = run_capture("test -e /dev/mapper/braid-disk2")
    assert exitcode != 0, "braid-disk2 mapper should be closed after replace"

with section("Data intact after live replace"):
    content = run("cat /mnt/storage/precious.txt").strip()
    assert content == "important data", f"Expected 'important data', got '{content}'"

with section("Disk map updated after live replace"):
    dm_raw = run("cat /var/lib/braid/disk-map.json")
    dm = json.loads(dm_raw)
    assert "disk2" not in dm["disks"], f"disk2 still in map: {dm}"
    assert "disk3" in dm["disks"], f"disk3 missing from map: {dm}"
    assert "disk1" in dm["disks"], f"disk1 missing from map: {dm}"

print("\nAll replace live canary tests passed.")
