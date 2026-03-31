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
    add_cmd, remove_cmd, disk, CONFIG, MOUNT_POINT,
)

# --- Phase 1: Build 3-drive RAID1 pool ---

with section("Setup: build 3-drive pool"):
    run(add_cmd("hwtest1", disk(1)))
    run(add_cmd("hwtest2", disk(2)))
    run(add_cmd("hwtest3", disk(3)))

    fi_show = run(f"btrfs fi show {MOUNT_POINT}")
    for name in ["braid-hwtest1", "braid-hwtest2", "braid-hwtest3"]:
        assert f"/dev/mapper/{name}" in fi_show, f"{name} missing:\n{fi_show}"

# --- Phase 2: Fill until dry-run rejects ---

with section("Fill pool until dry-run rejects removal"):
    iteration = 0
    fill_path = f"{MOUNT_POINT}/fill"
    threshold_crossed = False

    dry_cmd = remove_cmd("hwtest3", extra="--dry-run") + " 2>&1"

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
            run_capture("sync", timeout=60)
        else:
            run("sync")

        # Check dry-run
        dry_exit, dry_output = run_capture(dry_cmd, timeout=300)

        if dry_exit != 0 and "not enough space" in dry_output.lower():
            print(f"  Iteration {iteration}: dry-run rejected — threshold crossed")
            threshold_crossed = True
            break

        if dd_exit != 0:
            # Pool full but dry-run still passes — try smaller writes
            run_capture("sync", timeout=60)
            dry_exit2, dry_output2 = run_capture(dry_cmd, timeout=300)
            if dry_exit2 != 0 and "not enough space" in dry_output2.lower():
                print(f"  Iteration {iteration}: dry-run rejected after sync")
                threshold_crossed = True
                break

            # Fine-grained retry: 64 MB writes to nudge allocator
            print(f"  Iteration {iteration}: pool full but dry-run passes, trying 64 MB writes")
            for micro in range(1, 17):
                micro_cmd = (
                    f"dd if=/dev/zero of={fill_path}_micro_{micro} "
                    f"bs=1M count=64 2>&1"
                )
                run_capture(micro_cmd, timeout=120)
                run_capture("sync", timeout=60)

                dry_exit3, dry_output3 = run_capture(dry_cmd, timeout=300)
                if dry_exit3 != 0 and "not enough space" in dry_output3.lower():
                    print(f"  Micro write {micro}: dry-run rejected — threshold crossed")
                    threshold_crossed = True
                    break
            break

        print(f"  Iteration {iteration}: wrote 1 GB, dry-run still passes")

    dev_usage = run(f"btrfs device usage --raw {MOUNT_POINT}")
    print(f"\nDevice usage at threshold:\n{dev_usage}")

    assert threshold_crossed, (
        "Pool filled but braid remove --dry-run never produced ENOSPC rejection. "
        "Either dry-run has a bug or the test's fill strategy is insufficient."
    )

# --- Phase 3: Real remove also rejects ---

with section("Real remove rejects with ENOSPC"):
    real_cmd = remove_cmd("hwtest3") + " 2>&1"
    exit_code, output = run_capture(real_cmd, timeout=1800)

    print(f"braid remove output (exit {exit_code}):\n{output}")
    assert exit_code != 0, f"Expected failure, got exit 0: {output}"
    assert "not enough space" in output.lower(), \
        f"Expected 'not enough space' in error:\n{output}"

# --- Phase 4: Pool unchanged ---

with section("Pool still has all 3 devices (unchanged)"):
    fi_show = run(f"btrfs fi show {MOUNT_POINT}")
    print(f"Pool after rejection:\n{fi_show}")
    for name in ["braid-hwtest1", "braid-hwtest2", "braid-hwtest3"]:
        assert f"/dev/mapper/{name}" in fi_show, \
            f"{name} missing after rejection:\n{fi_show}"

# --- Phase 5: Filesystem still writable ---

with section("Filesystem still writable after rejection"):
    run(f"touch {MOUNT_POINT}/test-write")
    run(f"rm {MOUNT_POINT}/test-write")

print("\nRemove under pressure test passed.")
