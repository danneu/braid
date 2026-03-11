#!/usr/bin/env python3
"""
# Test: hardware canary — braid lock/unlock
#
# Intent: Revalidate `braid lock` (unmount + close all LUKS mappers) and
# `braid unlock` (open all LUKS + mount pool) on real hardware, including
# idempotency for both commands.
#
# Why it exists: VM tests verify lock/unlock on virtual devices where
# cryptsetup close/open are nearly instant. Real drives exercise actual
# LUKS open timing and real btrfs device scan + mount sequencing.
#
# Scenario: User locks a running pool (e.g. before detaching drives or
# powering down), then unlocks it later to resume access.
#
# Revalidates VM tests: tests/cli/braid-lock.py (tests 1-2),
#                        tests/cli/braid-unlock.py (tests 1-2)
"""

import sys
import os

sys.path.insert(0, os.path.dirname(__file__))
from harness import (
    run, run_fail, run_capture, cleanup, section,
    add_cmd, unlock_cmd, lock_cmd, MOUNT_POINT,
)

# --- Setup: Create 3-disk RAID1 pool ---

with section("Setup: create 3-disk pool"):
    run(add_cmd("hwtest1"))
    run(add_cmd("hwtest2"))
    run(add_cmd("hwtest3"))

    run(f"echo 'persistent data' > {MOUNT_POINT}/test.txt")
    run("sync")

# --- Test 1: Lock happy path ---

with section("Test 1: happy path — mounted pool locks cleanly"):
    run(f"mountpoint -q {MOUNT_POINT}")
    for k in ["hwtest1", "hwtest2", "hwtest3"]:
        run(f"test -e /dev/mapper/braid-{k}")

    run(lock_cmd())

    # Pool unmounted
    exitcode, _ = run_capture(f"mountpoint -q {MOUNT_POINT}")
    assert exitcode != 0, "Pool should be unmounted after lock"

    # All mappers closed
    for k in ["hwtest1", "hwtest2", "hwtest3"]:
        exitcode, _ = run_capture(f"test -e /dev/mapper/braid-{k}")
        assert exitcode != 0, f"Mapper braid-{k} should be closed after lock"

# --- Test 2: Lock idempotent ---

with section("Test 2: idempotent — lock again exits 0"):
    run(lock_cmd())

    exitcode, _ = run_capture(f"mountpoint -q {MOUNT_POINT}")
    assert exitcode != 0, "Pool should still be unmounted"
    for k in ["hwtest1", "hwtest2", "hwtest3"]:
        exitcode, _ = run_capture(f"test -e /dev/mapper/braid-{k}")
        assert exitcode != 0, f"Mapper braid-{k} should still be closed"

# --- Test 3: Unlock happy path ---

with section("Test 3: happy path — all locked, unlock opens everything"):
    run(unlock_cmd())

    run(f"mountpoint -q {MOUNT_POINT}")
    for k in ["hwtest1", "hwtest2", "hwtest3"]:
        run(f"test -e /dev/mapper/braid-{k}")

    content = run(f"cat {MOUNT_POINT}/test.txt").strip()
    assert content == "persistent data", f"Expected 'persistent data', got '{content}'"

# --- Test 4: Unlock idempotent ---

with section("Test 4: idempotent — unlock again is a no-op"):
    run(unlock_cmd())

    run(f"mountpoint -q {MOUNT_POINT}")
    content = run(f"cat {MOUNT_POINT}/test.txt").strip()
    assert content == "persistent data", f"Expected 'persistent data', got '{content}'"

print("\nAll lock/unlock canary tests passed.")
