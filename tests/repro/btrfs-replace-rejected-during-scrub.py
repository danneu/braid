# Repro: btrfs replace start is rejected when a scrub is in progress
#
# Intent: Confirm that `btrfs replace start` (in the exact argv shape braid
# invokes -- `--enqueue -r -f -B`) fails when a scrub is currently running on
# the same pool, AND that the resulting stderr contains the literal substring
# "scrub is in progress" that `cli/src/pool.rs::replace_error` classifies on.
#
# Why it exists: `--enqueue` does NOT wait scrub out -- scrub is not in btrfs'
# `exclusive_operation` set. Upstream returns
# `BTRFS_IOCTL_DEV_REPLACE_RESULT_SCRUB_INPROGRESS` and
# `replace_dev_result2string` (reference/btrfs-progs/cmds/replace.c:50-64)
# emits "scrub is in progress" in the START-ioctl error formatter
# (:330-356, gated by `do_not_background` -- i.e. -B). The braid classifier
# matches that exact substring (case-insensitive) to surface a recovery hint
# pointing at `btrfs scrub cancel` and `braid status`. A nixpkgs-bump-induced
# wording drift would silently misclassify in production unless this live-tool
# behavior-lock fails loudly first. This is the same pattern as
# `tests/repro/cryptsetup-close-mounted.py` documented in
# `docs/dev/testing.md#live-tool-behavior-locks`.
#
# Scenario: 2-of-3 btrfs RAID1 (disk1+disk2 pool, disk3 standby) on 1024 MiB
# disks. The live-scrub window comes from the kernel's per-device
# scrub_speed_max knob via the shared throttle helper
# (`tests/repro/scrub_throttle_helpers.py`): a 400 MiB payload at 20 MiB/s
# per device keeps each device's `dev->scrub_ctx` -- the bit
# `btrfs_dev_replace_start` -> `btrfs_scrub_dev` checks to return
# -EINPROGRESS / SCRUB_INPROGRESS -- live for a deterministic ~20 seconds
# (payload / rate), a comfortable window for the replace ioctl to land while
# scrub is in progress. The tool properties the throttle rests on are locked
# by `tests/repro/btrfs-scrub-limit-bounds-rate.py`. The pool is unencrypted:
# the rejection is a kernel ioctl result that never reads the block stack
# beneath btrfs, and the braid-stack-under-LUKS path has its own module-test
# coverage.
#
# The rejection itself is the precondition check: if the scrub finished
# early, the replace would start and the assertions below fail loudly. This
# test cannot go vacuously green.

import re

start_all()
machine.wait_for_unit("multi-user.target")

# --- Phase 1: Setup -- create RAID1 pool on disk1 + disk2 ---

with subtest("Setup: create 2-drive btrfs RAID1 pool"):
    d1 = "/dev/disk/by-id/virtio-disk1"
    d2 = "/dev/disk/by-id/virtio-disk2"
    machine.succeed(f"mkfs.btrfs -f -d raid1 -m raid1 {d1} {d2}")
    machine.succeed("mkdir -p /mnt/storage")
    machine.succeed(f"mount {d1} /mnt/storage")

    machine.succeed(
        "dd if=/dev/urandom of=/mnt/storage/payload bs=1M count=400 status=none"
    )
    machine.succeed("sync")

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print("Baseline btrfs fi show:\n" + fi_show)

    # Parse disk2's devid -- the device we'll attempt to replace. fi show
    # prints kernel device paths, so match on the by-id symlink's target.
    disk2_real = machine.succeed(f"readlink -f {d2}").strip()
    disk2_devid = None
    for line in fi_show.splitlines():
        if disk2_real in line:
            m = re.search(r"devid\s+(\d+)", line)
            assert m, "Could not parse devid from line: " + line
            disk2_devid = m.group(1)
            break
    assert disk2_devid is not None, (
        f"{disk2_real} not found in btrfs fi show:\n" + fi_show
    )
    print("disk2 devid: " + disk2_devid)

# --- Phase 2: Start a throttled scrub in the background ---
#
# After parent return, sleep briefly so the daemon child has a chance to
# issue BTRFS_IOC_SCRUB and the kernel allocates `dev->scrub_ctx` (checked
# at reference/linux/fs/btrfs/scrub.c#btrfs_scrub_dev). The throttled ~20s
# window keeps it set well past our one-second wait.

with subtest("Start throttled scrub in background"):
    scrub_throttle_start(machine, "/mnt/storage", rate_mib=20)
    machine.sleep(1)
    print("=== btrfs scrub status after 1s warm-up ===")
    print(machine.succeed("btrfs scrub status /mnt/storage"))

# --- Phase 3: Fire braid-shape btrfs replace start, expect rejection ---
#
# Use the exact argv shape braid emits via `CmdRequest::BtrfsReplaceStart`:
#   btrfs replace start --enqueue -r -f -B <devid> <target> <mount_point>
# The `-B` flag is load-bearing for this test: without it, `daemon(0, 0)`
# detaches before the START ioctl runs and the shell never sees the
# `"scrub is in progress"` error wording the classifier consumes
# (reference/btrfs-progs/cmds/replace.c:330-356).

with subtest("Attempt braid-shape btrfs replace -- expect scrub rejection"):
    stdout_path = "/tmp/btrfs-replace-during-scrub.out"
    stderr_path = "/tmp/btrfs-replace-during-scrub.err"
    cmd = (
        "btrfs replace start --enqueue -r -f -B "
        f"{disk2_devid} /dev/disk/by-id/virtio-disk3 /mnt/storage "
        f">{stdout_path} 2>{stderr_path}"
    )
    print("invoking: " + cmd)
    (status, _) = machine.execute(cmd)
    stdout = machine.succeed(f"cat {stdout_path}")
    stderr = machine.succeed(f"cat {stderr_path}")
    print("btrfs replace exit: " + str(status))
    print("btrfs replace stdout:\n" + stdout)
    print("btrfs replace stderr:\n" + stderr)

    assert status != 0, (
        f"Expected btrfs replace to FAIL during scrub but it exited {status}. "
        f"stdout:\n{stdout}\nstderr:\n{stderr}"
    )
    assert re.search(r"scrub is in progress", stderr, re.IGNORECASE), (
        "Expected stderr to contain 'scrub is in progress' (case-insensitive) "
        "-- the wording `cli/src/pool.rs::replace_error` classifies on. "
        f"stdout:\n{stdout}\nstderr:\n{stderr}"
    )
    print(
        "CONFIRMED: btrfs replace rejected during scrub with the marker "
        "substring intact"
    )

# --- Phase 4: Cancel scrub before shutdown so the VM teardown is clean ---

with subtest("Cancel scrub"):
    machine.succeed("btrfs scrub cancel /mnt/storage")

machine.shutdown()
