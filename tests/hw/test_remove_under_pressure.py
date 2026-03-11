#!/usr/bin/env python3
"""
# Test: hardware-only — remove pre-flight rejection under real space pressure
#
# Intent: Exercise braid's ENOSPC pre-flight rejection at real capacity where
# btrfs allocation has real fragmentation and real timing on 500 GB drives.
#
# Why it exists: VM ENOSPC tests (tests/cli/braid-remove-enospc.py) use
# 512 MiB drives where filling is instant and chunk allocation is trivial.
# This test validates the same pre-flight rejection at real capacity where
# btrfs chunk allocation patterns, fragmentation from iterative writes, and
# actual device usage on 500 GB drives may differ from VMs.
#
# Scenario: 3-disk RAID1 pool is gradually filled until braid's own dry-run
# pre-flight rejects removal, then the real remove is confirmed to also
# cleanly reject with the pool left unchanged and writable.
#
# No VM equivalent — hardware-only stress test.
"""

import sys
import os

sys.path.insert(0, os.path.dirname(__file__))
from harness import (
    run, run_fail, run_capture, cleanup, section,
    add_cmd, remove_cmd, CONFIG,
)

# --- Phase 1: Build 3-drive RAID1 pool ---

with section("Setup: build 3-drive pool"):
    run(add_cmd("disk1"))
    run(add_cmd("disk2"))
    run(add_cmd("disk3"))

    fi_show = run("btrfs fi show /mnt/storage")
    for name in ["braid-disk1", "braid-disk2", "braid-disk3"]:
        assert f"/dev/mapper/{name}" in fi_show, f"{name} missing:\n{fi_show}"

# --- Phase 2: Fill until dry-run rejects ---

with section("Fill pool until dry-run rejects removal"):
    iteration = 0
    fill_path = "/mnt/storage/fill"

    while True:
        iteration += 1

        # Write 1 GB
        dd_cmd = (
            f"dd if=/dev/zero of={fill_path}_{iteration} "
            f"bs=1M count=1024 status=progress 2>&1"
        )
        dd_exit, dd_output = run_capture(dd_cmd, timeout=600)

        if dd_exit != 0:
            print(f"  Iteration {iteration}: dd failed (pool full), stopping fill")
            # Sync whatever was written
            run_capture("sync", timeout=60)
        else:
            run("sync")

        # Check dry-run
        dry_cmd = remove_cmd("disk3", extra="--dry-run") + " 2>&1"
        dry_exit, dry_output = run_capture(dry_cmd, timeout=300)

        if dry_exit != 0 and "not enough space" in dry_output.lower():
            print(f"  Iteration {iteration}: dry-run rejected — threshold crossed")
            break

        if dd_exit != 0:
            # Pool is full but dry-run didn't reject yet — try one more sync
            run_capture("sync", timeout=60)
            dry_exit2, dry_output2 = run_capture(dry_cmd, timeout=300)
            if dry_exit2 != 0 and "not enough space" in dry_output2.lower():
                print(f"  Iteration {iteration}: dry-run rejected after sync")
                break
            # Pool full but dry-run still passes — remove some data to keep going
            # shouldn't happen on 500 GB drives, but be safe
            print(f"  WARNING: pool full but dry-run still passes")
            print(f"  dry-run output: {dry_output2}")
            break

        print(f"  Iteration {iteration}: wrote 1 GB, dry-run still passes")

    dev_usage = run("btrfs device usage --raw /mnt/storage")
    print(f"\nDevice usage at threshold:\n{dev_usage}")

# --- Phase 3: Real remove also rejects ---

with section("Real remove rejects with ENOSPC"):
    real_cmd = remove_cmd("disk3") + " 2>&1"
    exit_code, output = run_capture(real_cmd, timeout=1800)

    print(f"braid remove output (exit {exit_code}):\n{output}")
    assert exit_code != 0, f"Expected failure, got exit 0: {output}"
    assert "not enough space" in output.lower(), \
        f"Expected 'not enough space' in error:\n{output}"

# --- Phase 4: Pool unchanged ---

with section("Pool still has all 3 devices (unchanged)"):
    fi_show = run("btrfs fi show /mnt/storage")
    print(f"Pool after rejection:\n{fi_show}")
    for name in ["braid-disk1", "braid-disk2", "braid-disk3"]:
        assert f"/dev/mapper/{name}" in fi_show, \
            f"{name} missing after rejection:\n{fi_show}"

# --- Phase 5: Filesystem still writable ---

with section("Filesystem still writable after rejection"):
    run("touch /mnt/storage/test-write")
    run("rm /mnt/storage/test-write")

print("\nRemove under pressure test passed.")
