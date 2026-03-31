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
    add_cmd, replace_cmd, disk, CONFIG, MOUNT_POINT,
)

# --- Phase 0: Build 2-drive RAID1 pool ---

with section("Setup: build 2-drive RAID1 pool"):
    run(add_cmd("hwtest1", disk(1)))
    run(add_cmd("hwtest2", disk(2)))

    fi_show = run(f"btrfs fi show {MOUNT_POINT}")
    for name in ["braid-hwtest1", "braid-hwtest2"]:
        assert f"/dev/mapper/{name}" in fi_show, f"{name} missing:\n{fi_show}"

    df_output = run(f"btrfs fi df {MOUNT_POINT}")
    assert "RAID1" in df_output, f"Expected RAID1 profile:\n{df_output}"

    run(f"echo 'important data' > {MOUNT_POINT}/precious.txt")
    run("sync")

# --- Phase 1: Live replace disk2 → disk3 ---

with section("Live replace hwtest2 with hwtest3"):
    result = run(replace_cmd("hwtest2", "hwtest3", disk(3)), timeout=1800)
    print(f"braid replace output:\n{result}")

with section("Pool healthy after live replace"):
    fi_show = run(f"btrfs fi show {MOUNT_POINT}")
    print(f"Pool after live replace:\n{fi_show}")

    assert "/dev/mapper/braid-hwtest3" in fi_show, \
        f"New disk braid-hwtest3 missing from pool:\n{fi_show}"
    assert "braid-hwtest2" not in fi_show, \
        f"Old disk braid-hwtest2 should be removed:\n{fi_show}"
    assert "missing" not in fi_show.lower(), \
        f"Pool should have no missing devices:\n{fi_show}"
    assert "/dev/mapper/braid-hwtest1" in fi_show, \
        f"braid-hwtest1 missing from pool:\n{fi_show}"

    devid_count = fi_show.count("devid")
    assert devid_count == 2, f"Expected 2 devices, got {devid_count}:\n{fi_show}"

    df_output = run(f"btrfs fi df {MOUNT_POINT}")
    assert "RAID1" in df_output, f"Expected RAID1 profile:\n{df_output}"

with section("Old disk LUKS mapper closed"):
    exitcode, _ = run_capture("test -e /dev/mapper/braid-hwtest2")
    assert exitcode != 0, "braid-hwtest2 mapper should be closed after replace"

with section("Data intact after live replace"):
    content = run(f"cat {MOUNT_POINT}/precious.txt").strip()
    assert content == "important data", f"Expected 'important data', got '{content}'"

with section("Pool membership updated after live replace"):
    pm_raw = run("cat /var/lib/braid/pool.json")
    pm = json.loads(pm_raw)
    assert "hwtest2" not in pm["disks"], f"hwtest2 still in pool: {pm}"
    assert "hwtest3" in pm["disks"], f"hwtest3 missing from pool: {pm}"
    assert "hwtest1" in pm["disks"], f"hwtest1 missing from pool: {pm}"

print("\nAll replace live canary tests passed.")
