# Repro: btrfs scrub start/resume are rejected while a scrub is already running
#
# Intent: Confirm that `btrfs scrub start -B` AND `btrfs scrub resume -B` (the
# exact argv shapes braid invokes) exit non-zero while another scrub is running
# on the same pool, AND that both stderrs contain the literal substring
# "Scrub is already running." that
# `cli/src/scrub_resume_or_start.rs::classify_btrfs_failure` classifies on.
#
# Why it exists: braid's scheduled-scrub gate probes `btrfs scrub status`
# before spawning, but an external scrub can start in the window between that
# probe and braid's own invocation. braid recognizes the lost race solely from
# btrfs's refusal wording -- a post-failure status re-probe is racy in both
# directions -- and downgrades it from a scrub-failed alert to a busy skip.
# That makes the wording load-bearing: a nixpkgs-bump-induced drift would turn
# every lost race back into a spurious 3am beep while every mocked test still
# passed. Upstream emits it from `is_scrub_running_on_fs` in `scrub_start`
# (reference/btrfs-progs/cmds/scrub.c), which `cmd_scrub_start` and
# `cmd_scrub_resume` both call, and which runs before the `do_background` fork
# -- so `-B` is not what makes the parent see the error, but it is the shape
# braid uses. Same pattern as `tests/repro/cryptsetup-close-mounted.py`,
# documented in `docs/dev/testing.md#live-tool-behavior-locks`.
#
# Scenario: an operator kicked off a hand scrub; braid's timer fires seconds
# later and its own resume/start is refused. Sizing is copied from the sibling
# `tests/repro/btrfs-replace-rejected-during-scrub.py`: 2-of-2 LUKS + btrfs
# RAID1 on 4096 MiB disks with a 3000 MiB urandom payload, which at
# linux-builder's observed ~400 MiB/s scrub rate keeps the scrub live for
# ~7-15 seconds. The LUKS layer is not scenery here -- it is the throttle. An
# unencrypted pool on this builder scrubs the same payload in ~1.5 seconds,
# which is not a window this test can land in. `btrfs scrub start --limit` was
# tried instead and did not slow the kernel scrub at all.
#
# The refusals themselves are the precondition check: if the scrub finished
# early, `btrfs scrub resume -B` returns "nothing to resume" and `start -B`
# succeeds, and the assertions below fail loudly. This test cannot go
# vacuously green.

import re

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
luks_format = "--batch-mode --key-file=- --pbkdf pbkdf2 --pbkdf-force-iterations 1000"

# --- Phase 1: Setup -- LUKS + btrfs RAID1 with a payload big enough to scrub ---

with subtest("Setup: create a 2-drive LUKS + btrfs RAID1 pool"):
    for name in ["disk1", "disk2"]:
        dev = f"/dev/disk/by-id/virtio-{name}"
        machine.succeed(f"echo -n '{passphrase}' | cryptsetup luksFormat {luks_format} {dev}")
        machine.succeed(f"echo -n '{passphrase}' | cryptsetup luksOpen --key-file=- {dev} {name}")

    machine.succeed("mkfs.btrfs -f -d raid1 -m raid1 /dev/mapper/disk1 /dev/mapper/disk2")
    machine.succeed("mkdir -p /mnt/storage")
    machine.succeed("mount /dev/mapper/disk1 /mnt/storage")
    machine.succeed(
        "dd if=/dev/urandom of=/mnt/storage/payload bs=1M count=3000 status=none"
    )
    machine.succeed("sync")

# --- Phase 2: Start a scrub in the background, as a hand-run scrub would ---
#
# `btrfs scrub start` (no -B) forks a daemon child that holds the inherited
# stdout open until scrub completes. The NixOS test driver waits for stdout to
# close on every `machine.execute` call, so without redirecting we would block
# here for the full scrub. Redirecting to /dev/null lets `machine.succeed`
# return as soon as the parent fork-and-exits; the kernel scrub keeps running.
#
# The parent writes the all-zero progress record to
# `/var/lib/btrfs/scrub.status.<fsid>` *before* it forks, so the record
# `is_scrub_running_on_fs` reads (finished == 0 && canceled == 0) is already in
# place when the parent returns. Only the kernel side needs the brief warm-up.
# Note that `btrfs scrub status` still prints "no stats available" at this
# point -- the child has not yet stamped `t_start` -- so the printed status is
# NOT a usable precondition probe; the refusals below are.

with subtest("Start a scrub in the background"):
    machine.succeed("btrfs scrub start /mnt/storage > /dev/null 2>&1")
    machine.sleep(1)
    print("=== btrfs scrub status after 1s warm-up ===")
    print(machine.succeed("btrfs scrub status /mnt/storage"))

# --- Phase 3: Both braid-shape invocations must be refused with the wording ---

REJECTION = "Scrub is already running."


def assert_rejected(subcommand):
    stdout_path = f"/tmp/btrfs-scrub-{subcommand}.out"
    stderr_path = f"/tmp/btrfs-scrub-{subcommand}.err"
    cmd = f"btrfs scrub {subcommand} -B /mnt/storage >{stdout_path} 2>{stderr_path}"
    print("invoking: " + cmd)
    (status, _) = machine.execute(cmd)
    stdout = machine.succeed(f"cat {stdout_path}")
    stderr = machine.succeed(f"cat {stderr_path}")
    print(f"btrfs scrub {subcommand} exit: {status}")
    print(f"btrfs scrub {subcommand} stdout:\n{stdout}")
    print(f"btrfs scrub {subcommand} stderr:\n{stderr}")

    assert status != 0, (
        f"Expected `btrfs scrub {subcommand} -B` to FAIL over a running scrub "
        f"but it exited {status}. stdout:\n{stdout}\nstderr:\n{stderr}"
    )
    assert re.search(re.escape(REJECTION), stderr), (
        f"Expected stderr to contain {REJECTION!r} -- the wording "
        "`cli/src/scrub_resume_or_start.rs` classifies a lost race on. "
        f"stdout:\n{stdout}\nstderr:\n{stderr}"
    )
    print(f"CONFIRMED: `btrfs scrub {subcommand} -B` refused with the marker substring")


with subtest("btrfs scrub resume -B is refused with the already-running wording"):
    # braid tries resume first, so the resume arm is the one a lost race hits
    # most often; `cmd_scrub_resume` calls the same `scrub_start` guard.
    assert_rejected("resume")

with subtest("btrfs scrub start -B is refused with the same wording"):
    # The fallback arm braid reaches when there is nothing to resume.
    assert_rejected("start")

# --- Phase 4: Cancel the scrub so VM teardown is clean ---

with subtest("Cancel scrub"):
    machine.succeed("btrfs scrub cancel /mnt/storage")

machine.shutdown()
