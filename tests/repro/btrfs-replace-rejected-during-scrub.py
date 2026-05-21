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
# `docs/testing.md:64-72`.
#
# Scenario: 2-of-3 LUKS+btrfs RAID1 (disk1+disk2 pool, disk3 standby) on
# 4096 MiB disks, with a 3000 MiB urandom payload. Scrub runs unthrottled
# (the per-device sysfs `scrub_speed_max` knob would race with the daemon
# child's restore-old-limit loop in cmds/scrub.c:1600 and only throttles
# the first device), so we rely on payload size: at linux-builder's
# observed ~400 MiB/s scrub rate, ~3 GiB of scrub work per disk keeps
# `dev->scrub_ctx` live for ~7-15 seconds in parallel on both devices.
# That window is comfortably large for the replace ioctl to land while
# scrub is in progress. Stdio for `btrfs scrub start` is redirected to
# `/dev/null` so the parent's return is not held back by the child holding
# the inherited stdout open (the NixOS test driver waits for stdout to
# close on every machine.execute call).

import re

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
luks_format = "--batch-mode --key-file=- --pbkdf pbkdf2 --pbkdf-force-iterations 1000"

# --- Phase 1: Setup -- LUKS format + open disk1 and disk2, create RAID1 pool ---

with subtest("Setup: create 2-drive LUKS + btrfs RAID1 pool"):
    for name in ["disk1", "disk2"]:
        dev = f"/dev/disk/by-id/virtio-{name}"
        machine.succeed(f"echo -n '{passphrase}' | cryptsetup luksFormat {luks_format} {dev}")
        machine.succeed(f"echo -n '{passphrase}' | cryptsetup luksOpen --key-file=- {dev} {name}")

    machine.succeed(
        "mkfs.btrfs -f -d raid1 -m raid1"
        " /dev/mapper/disk1"
        " /dev/mapper/disk2"
    )
    machine.succeed("mkdir -p /mnt/storage")
    machine.succeed("mount /dev/mapper/disk1 /mnt/storage")

    # 3 GiB payload across 2 RAID1 mirrors -- ~3 GiB of scrub work per
    # disk. At linux-builder's observed ~400 MiB/s unthrottled scrub rate,
    # that keeps each device's `dev->scrub_ctx` live for ~7-8 seconds.
    machine.succeed(
        "dd if=/dev/urandom of=/mnt/storage/payload bs=1M count=3000 status=none"
    )
    machine.succeed("sync")

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print("Baseline btrfs fi show:\n" + fi_show)

    # Parse disk2's devid -- the device we'll attempt to replace.
    disk2_devid = None
    for line in fi_show.splitlines():
        if "/dev/mapper/disk2" in line:
            m = re.search(r"devid\s+(\d+)", line)
            assert m, "Could not parse devid from line: " + line
            disk2_devid = m.group(1)
            break
    assert disk2_devid is not None, "disk2 not found in btrfs fi show:\n" + fi_show
    print("disk2 devid: " + disk2_devid)

# --- Phase 2: LUKS prep disk3 (the standby replacement target) ---

with subtest("LUKS prep disk3 (standby replacement target)"):
    dev3 = "/dev/disk/by-id/virtio-disk3"
    machine.succeed(f"echo -n '{passphrase}' | cryptsetup luksFormat {luks_format} {dev3}")
    machine.succeed(f"echo -n '{passphrase}' | cryptsetup luksOpen --key-file=- {dev3} disk3")

# --- Phase 3: Start scrub in background ---
#
# `btrfs scrub start` (no -B) forks a daemon child that holds the inherited
# stdout open until scrub completes. The NixOS test driver waits for stdout
# to close on every `machine.execute` call (see machine/__init__.py docstring
# for `execute`), so without redirecting we'd block here for the full scrub
# duration. Redirecting to /dev/null detaches the child from the driver's
# stdout pipe and lets `machine.succeed` return as soon as the parent
# fork-and-exits, which is essentially instant. The kernel scrub continues
# running on both devices in parallel.
#
# After parent return, sleep briefly so the daemon child has a chance to
# issue BTRFS_IOC_SCRUB and the kernel allocates `dev->scrub_ctx` -- the
# bit `btrfs_dev_replace_start` -> `btrfs_scrub_dev` checks at
# reference/linux/fs/btrfs/scrub.c:3003 to decide whether to return
# -EINPROGRESS / SCRUB_INPROGRESS. With ~3 GiB of work per disk at ~400
# MiB/s, scrub_ctx stays set for many seconds, well past our half-second
# wait.

with subtest("Start scrub in background"):
    machine.succeed("btrfs scrub start /mnt/storage > /dev/null 2>&1")
    machine.sleep(1)
    print("=== btrfs scrub status after 1s warm-up ===")
    print(machine.succeed("btrfs scrub status /mnt/storage"))

# --- Phase 4: Fire braid-shape btrfs replace start, expect rejection ---
#
# Use the exact argv shape braid emits via `CmdRequest::BtrfsReplaceStart`:
#   btrfs replace start --enqueue -r -f -B <devid> <target> <mount_point>
# The `-B` flag is load-bearing for this test: without it, `daemon(0, 0)`
# detaches before the START ioctl runs and the shell never sees the
# `"scrub is in progress"` error wording the classifier consumes
# (reference/btrfs-progs/cmds/replace.c:330-356).

with subtest("Attempt braid-shape btrfs replace -- expect scrub rejection"):
    cmd = (
        "btrfs replace start --enqueue -r -f -B "
        f"{disk2_devid} /dev/mapper/disk3 /mnt/storage 2>&1"
    )
    print("invoking: " + cmd)
    (status, output) = machine.execute(cmd)
    print("btrfs replace exit: " + str(status))
    print("btrfs replace stderr/stdout:\n" + output)

    assert status != 0, (
        "Expected btrfs replace to FAIL during scrub but it exited "
        + str(status) + ". Output:\n" + output
    )
    assert re.search(r"scrub is in progress", output, re.IGNORECASE), (
        "Expected stderr to contain 'scrub is in progress' (case-insensitive) "
        "-- the wording `cli/src/pool.rs::replace_error` classifies on. "
        "Output:\n" + output
    )
    print(
        "CONFIRMED: btrfs replace rejected during scrub with the marker "
        "substring intact"
    )

# --- Phase 5: Cancel scrub before shutdown so the VM teardown is clean ---

with subtest("Cancel scrub"):
    machine.succeed("btrfs scrub cancel /mnt/storage")

machine.shutdown()
