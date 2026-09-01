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
# later and its own resume/start is refused. The live-scrub window comes from
# the kernel's per-device scrub_speed_max knob via the shared throttle helper
# (`tests/repro/scrub_throttle_helpers.py`): a 400 MiB payload on a 2-drive
# btrfs RAID1 at 20 MiB/s per device is a deterministic ~20 second window
# (payload / rate), comfortably larger than the refusal assertions need. The
# tool properties the throttle rests on are locked by
# `tests/repro/btrfs-scrub-limit-bounds-rate.py`. The pool is unencrypted:
# the refusal wording comes from btrfs-progs' status-file check and never
# reads the block stack beneath btrfs, and the braid-stack-under-LUKS path
# has its own module-test coverage.
#
# The refusals themselves are the precondition check: if the scrub finished
# early, `btrfs scrub resume -B` returns "nothing to resume" and `start -B`
# succeeds, and the assertions below fail loudly. This test cannot go
# vacuously green.

import re

start_all()
machine.wait_for_unit("multi-user.target")

# --- Phase 1: Setup -- btrfs RAID1 with a payload to scrub ---

with subtest("Setup: create a 2-drive btrfs RAID1 pool"):
    d1 = "/dev/disk/by-id/virtio-disk1"
    d2 = "/dev/disk/by-id/virtio-disk2"
    machine.succeed(f"mkfs.btrfs -f -d raid1 -m raid1 {d1} {d2}")
    machine.succeed("mkdir -p /mnt/storage")
    machine.succeed(f"mount {d1} /mnt/storage")
    machine.succeed(
        "dd if=/dev/urandom of=/mnt/storage/payload bs=1M count=400 status=none"
    )
    machine.succeed("sync")

# --- Phase 2: Start a throttled scrub in the background, as a hand-run scrub would ---
#
# The parent writes the all-zero progress record to
# `/var/lib/btrfs/scrub.status.<fsid>` *before* it forks, so the record
# `is_scrub_running_on_fs` reads (finished == 0 && canceled == 0) is already in
# place when the parent returns. Only the kernel side needs the brief warm-up.
# Note that `btrfs scrub status` still prints "no stats available" at this
# point -- the child has not yet stamped `t_start` -- so the printed status is
# NOT a usable precondition probe; the refusals below are.

with subtest("Start a throttled scrub in the background"):
    scrub_throttle_start(machine, "/mnt/storage", rate_mib=20)
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
