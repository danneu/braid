# Repro: btrfs's own scrub record is a usable scheduling anchor
#
# Intent: Confirm the two upstream properties braid's scrub scheduler depends
# on: (1) a scrub run by hand -- `btrfs scrub start -B`, no braid anywhere in
# the picture -- moves the `Scrub started:` timestamp that `btrfs scrub status`
# reports, and (2) that record survives a reboot, still reporting `finished`
# with the same timestamp.
#
# Why it exists: `classify_freshness` in `cli/src/scrub_resume_or_start.rs`
# treats that timestamp as the single scheduling anchor (ADR 035). Both halves
# are load-bearing and neither is braid's own behavior:
#   * If a hand scrub did not move it, "your own scrub counts and pushes the
#     next automatic scrub out" -- the headline promise of the redesign -- would
#     be false, and braid would re-scrub a pool the operator just scrubbed.
#   * If the record did not survive a reboot, every boot would read as
#     never-scrubbed and scrub the pool, which is exactly the runaway the old
#     timer stamp file existed to prevent. Deleting that stamp is only safe
#     because btrfs's record persists in its place.
# btrfs writes it to `/var/lib/btrfs/scrub.status.<fsid>` from the userspace
# side of any scrub, whoever started it (reference/btrfs-progs/cmds/scrub.c).
# A nixpkgs bump that changed either property would leave every mocked test
# green while the scheduler quietly reverted to scrubbing on every poll.
# Same pattern as `tests/repro/cryptsetup-close-mounted.py`, documented in
# `docs/dev/testing.md#live-tool-behavior-locks`.
#
# Scenario: an operator scrubs their pool by hand on the 30th, reboots the NAS,
# and expects braid not to scrub it again on the 1st.
#
# The test cannot go vacuously green: each phase asserts on a concrete
# timestamp parsed out of the status output, and the never-scrubbed precondition
# is checked before the first scrub runs.

import re

start_all()
machine.wait_for_unit("multi-user.target")

MOUNT = "/mnt/storage"
ANCHOR_RE = re.compile(r"Scrub (?:started|resumed):\s+(.+)")


def anchor():
    """The timestamp braid schedules from: btrfs's latest start-or-resume."""
    status = machine.succeed(f"btrfs scrub status --raw {MOUNT}")
    match = ANCHOR_RE.search(status)
    assert match, f"no Scrub started/resumed line in status:\n{status}"
    return match.group(1).strip()


def assert_finished():
    status = machine.succeed(f"btrfs scrub status --raw {MOUNT}")
    assert re.search(r"Status:\s+finished", status), (
        f"expected a finished scrub, got:\n{status}"
    )
    return status


def mount_pool():
    machine.succeed(f"mkdir -p {MOUNT}")
    machine.succeed("btrfs device scan")
    machine.succeed(f"mount /dev/disk/by-id/virtio-disk1 {MOUNT}")


# --- Phase 1: a pool that has never been scrubbed ---

with subtest("Setup: a 2-drive btrfs RAID1 pool with no scrub history"):
    machine.succeed(
        "mkfs.btrfs -f -d raid1 -m raid1 "
        "/dev/disk/by-id/virtio-disk1 /dev/disk/by-id/virtio-disk2"
    )
    mount_pool()
    machine.succeed(f"dd if=/dev/urandom of={MOUNT}/payload bs=1M count=64 status=none")
    machine.succeed("sync")

    status = machine.succeed(f"btrfs scrub status --raw {MOUNT}")
    print(f"=== status before any scrub ===\n{status}")
    assert "no stats available" in status, (
        "a never-scrubbed pool must report no stats -- the precondition this "
        f"test's later assertions are measured against. Got:\n{status}"
    )

# --- Phase 2: a hand-run scrub moves the anchor ---

with subtest("A hand-run scrub records a start timestamp"):
    machine.succeed(f"btrfs scrub start -B {MOUNT}")
    print(f"=== status after the first hand scrub ===\n{assert_finished()}")
    first = anchor()
    print(f"first anchor: {first}")

with subtest("A second hand-run scrub moves the anchor forward"):
    # The whole point: braid started neither of these. If btrfs did not move the
    # timestamp for a scrub braid knows nothing about, an operator's own scrub
    # could not suppress the next automatic one.
    #
    # btrfs renders the anchor at one-second resolution, so wait past a second
    # boundary -- otherwise a fast scrub can legitimately report the same
    # timestamp and this assertion would fail on timing, not on behavior.
    machine.sleep(2)
    machine.succeed(f"btrfs scrub start -B {MOUNT}")
    print(f"=== status after the second hand scrub ===\n{assert_finished()}")
    second = anchor()
    print(f"second anchor: {second}")
    assert second != first, (
        "a hand-run scrub must move the scheduling anchor, but it stayed at "
        f"{first!r}. braid would re-scrub a pool the operator just scrubbed."
    )

# --- Phase 3: the record survives a reboot ---

with subtest("The scrub record survives a reboot"):
    machine.succeed("sync")
    machine.shutdown()
    machine.start()
    machine.wait_for_unit("multi-user.target")
    mount_pool()

    print(f"=== status after reboot ===\n{assert_finished()}")
    after_reboot = anchor()
    print(f"anchor after reboot: {after_reboot}")
    assert after_reboot == second, (
        "the scrub record must survive a reboot unchanged: braid deleted its "
        "own timer stamp file on the strength of this. Expected "
        f"{second!r}, got {after_reboot!r}."
    )

machine.shutdown()
