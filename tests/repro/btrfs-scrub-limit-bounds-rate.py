# Repro: bounded-rate scrub launch keeps every device at the configured rate
#
# Intent: Lock the three btrfs-progs/kernel properties every throttled
# scrub-window test rests on:
#   1. /sys/fs/btrfs/<fsid>/devinfo/<devid>/scrub_speed_max exists, and a
#      scrub launched the way the shared test helper launches -- rate
#      persisted with `btrfs scrub limit -a -l <rate>`, scrub started with
#      `btrfs scrub start --limit <same rate>` -- is actually bounded: a known
#      payload takes at least payload/rate wall time.
#   2. That launch shape is both necessary and sufficient, observed per
#      device, not in aggregate. `scrub_start` saves each device's old limit,
#      writes its --limit value (0 when the flag is absent) to *every* device,
#      spawns the scrub threads, then restores each saved value only just
#      before that device's pthread_join -- devid 1 within milliseconds, devid
#      N only after devids 1..N-1 finish scrubbing
#      (reference/btrfs-progs/cmds/scrub.c#scrub_start; the kernel reads the
#      knob live per bio, reference/linux/fs/btrfs/scrub.c#scrub_throttle_dev_io).
#      So with a rate preconfigured via `scrub limit`, a plain `scrub start`
#      leaves the last device's knob reading 0 while the first reads the
#      configured rate -- every device but the first runs unlimited -- whereas
#      a `--limit <same rate>` launch writes that rate in both the temporary
#      write and the restore, so every device's knob holds it for the whole
#      run.
#   3. The configured limit is intact after a run: the restore puts the saved
#      value back, it does not clear it.
#
# Why it exists: tests that need a scrub to stay running for a window of
# seconds get that window from this knob (window = payload / rate). A
# btrfs-progs or kernel pin bump that removed the knob, stopped the --limit
# write from landing, or changed the restore ordering would silently turn
# those deterministic windows back into builder-speed folklore while every
# throttled test stayed green. This repro is the manual pin-bump gate named in
# docs/dev/parser-compatibility.md -- repro-* checks are excluded from
# `.#checks` and no CI workflow runs them. Same pattern as
# `tests/repro/cryptsetup-close-mounted.py`, documented in
# `docs/dev/testing.md#live-tool-behavior-locks`.
#
# Scenario: a nixpkgs bump moves the pinned btrfs-progs/kernel pair; the
# pin-bump procedure runs `just test-repro`, and this lock fails loudly
# instead of the scrub-refusal repros' live-scrub windows quietly collapsing.
#
# If the "plain scrub start discards the preconfigured limit" subtest fires:
# btrfs-progs has likely fixed the revert-before-join ordering (an upstream
# bug as of 6.19.1). That is an improvement, not a regression. Re-read
# `reference/btrfs-progs/cmds/scrub.c#scrub_start` on the new pin; if a
# preconfigured `scrub limit` now stays in force through a plain start, the
# shared helper's own-the-launch requirement can be relaxed -- update the
# helper and this lock together. The throttled tests themselves are safe
# either way: a `--limit <same rate>` launch is correct under either ordering.

import time

start_all()
machine.wait_for_unit("multi-user.target")

MOUNT = "/mnt/storage"
RATE_ARG = "20m"
RATE_BYTES = 20 * 1024 * 1024
PAYLOAD_MIB = 400
# RAID1: each device holds a full copy of the payload and scrubs it at
# RATE_BYTES/s in parallel, so the nominal wall time is 400/20 = 20s. Assert
# half of that -- generous against rate overshoot, and far above the ~1s an
# unthrottled scrub of this payload takes on the builder.
FLOOR_SECONDS = (PAYLOAD_MIB * 1024 * 1024) / RATE_BYTES / 2


def knobs():
    """Both devices' scrub_speed_max values, in devid order."""
    return machine.succeed(f"cat {KNOB1} {KNOB2}").split()


def wait_scrub_finished(t0):
    """Poll until the scrub reports finished; return the wall time since t0."""
    status = ""
    for _ in range(120):
        status = machine.succeed(f"btrfs scrub status {MOUNT}")
        if "finished" in status:
            return time.time() - t0
        time.sleep(1)
    raise AssertionError("scrub did not finish within 120s:\n" + status)


# --- Setup ---

with subtest("Setup: 2-drive btrfs RAID1, payload, sysfs knobs exist"):
    d1 = "/dev/disk/by-id/virtio-disk1"
    d2 = "/dev/disk/by-id/virtio-disk2"
    machine.succeed(f"mkfs.btrfs -f -d raid1 -m raid1 {d1} {d2}")
    machine.succeed(f"mkdir -p {MOUNT}")
    machine.succeed(f"mount {d1} {MOUNT}")
    machine.succeed(
        f"dd if=/dev/urandom of={MOUNT}/payload bs=1M count={PAYLOAD_MIB} status=none"
    )
    machine.succeed("sync")
    fsid = machine.succeed(
        f"btrfs filesystem show {MOUNT} | sed -n 's/.*uuid: //p'"
    ).strip()
    KNOB1 = f"/sys/fs/btrfs/{fsid}/devinfo/1/scrub_speed_max"
    KNOB2 = f"/sys/fs/btrfs/{fsid}/devinfo/2/scrub_speed_max"
    # Property 1, first half: the knob exists. A pin where it vanished dies
    # here instead of letting throttled tests go slow-path green.
    initial = knobs()
    print("initial knob values: " + repr(initial))

# --- Property 1 + 2 (sufficient) + 3: the helper's launch shape ---

with subtest("scrub limit + start --limit same rate: every device bounded, whole run"):
    machine.succeed(f"btrfs scrub limit -a -l {RATE_ARG} {MOUNT}")
    configured = knobs()
    assert configured == [str(RATE_BYTES)] * 2, (
        "`scrub limit -a -l` must set every device's scrub_speed_max; got "
        + repr(configured)
    )

    t0 = time.time()
    # The non-blocking form forks; the child inherits stdout, so without the
    # redirect machine.succeed would sit on the open pipe until the scrub
    # child exits -- turning every "mid-run" sample below into a post-run one.
    machine.succeed(f"btrfs scrub start --limit {RATE_ARG} {MOUNT} >/dev/null 2>&1")

    # Per-device, not aggregate: with --limit <rate>, both the temporary write
    # and each device's restore write the same rate, so at no point may any
    # device's knob read anything else -- these samples cannot race.
    early = knobs()
    assert early == [str(RATE_BYTES)] * 2, (
        "every device must be bounded from scrub-thread launch onward; "
        "knobs just after start: " + repr(early)
    )
    time.sleep(FLOOR_SECONDS / 2)
    mid = knobs()
    assert mid == [str(RATE_BYTES)] * 2, (
        "every device must stay bounded for the whole run; knobs mid-run: "
        + repr(mid)
    )

    wall = wait_scrub_finished(t0)
    print(f"throttled scrub wall time: {wall:.2f}s (floor {FLOOR_SECONDS:.0f}s)")
    assert wall >= FLOOR_SECONDS, (
        "the knob must actually bound scrub rate: "
        + f"{PAYLOAD_MIB} MiB per device at {RATE_ARG}/s finished in {wall:.2f}s, "
        + f"below the {FLOOR_SECONDS:.0f}s floor. The throttle is not in force."
    )

    # Property 3: the run's restore puts the configured value back.
    after = knobs()
    assert after == [str(RATE_BYTES)] * 2, (
        "the configured limit must survive a run (restore, not clear); knobs "
        "after finish: " + repr(after)
    )

# --- Property 2 (necessary), the restore-ordering canary ---

with subtest("plain scrub start discards the preconfigured limit on all but devid 1"):
    machine.succeed(f"btrfs scrub limit -a -l {RATE_ARG} {MOUNT}")
    t0 = time.time()
    # Same stdout-inheritance gotcha as above: without the redirect this call
    # would block until the scrub finished and the poll below would only ever
    # see the post-run restored state.
    machine.succeed(f"btrfs scrub start {MOUNT} >/dev/null 2>&1")

    # Expect the asymmetric state: devid 1 already restored to the configured
    # rate (milliseconds after launch), devid 2 still at progs' temporary
    # write of 0 -- its restore waits until devid 1, now throttled to ~20s,
    # finishes. Poll briefly to step over the launch-instant [0, 0] state.
    deadline = time.time() + 5
    seen = knobs()
    while seen != [str(RATE_BYTES), "0"] and time.time() < deadline:
        time.sleep(0.2)
        seen = knobs()
    assert seen == [str(RATE_BYTES), "0"], (
        "canary: expected devid 1 restored to the configured rate and devid 2 "
        "left at 0 shortly after a plain `scrub start`; got " + repr(seen) + ". "
        "btrfs-progs' restore ordering has likely changed -- see this file's "
        "preamble for what to do."
    )

    wait_scrub_finished(t0)
    # Property 3 on the plain path too: the sequential restores put the
    # configured value back on every device once the run is over.
    after_plain = knobs()
    assert after_plain == [str(RATE_BYTES)] * 2, (
        "after a plain start the restores must reinstate the configured "
        "limit; knobs after finish: " + repr(after_plain)
    )

machine.shutdown()
