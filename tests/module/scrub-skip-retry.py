# Test: scrub-skip-retry
#
# Intent: Verify the busy-scrub gate end to end -- a scheduled scrub that fires
#   while a balance is in flight exits 4 without starting any scrub and without
#   touching scrub state; the skip raises no alert (no scrub-failed flag, no
#   beeper, no alertCommand); the *next timer poll* runs the scrub for real once
#   the pool is clear; and the poll after that finds the pool fresh and exits 0
#   without touching anything.
#
# Why it exists: on caja a `braid add` convert balance was mid-flight with the
#   monthly scrub due at midnight and nothing stopped the scrub from piling onto
#   the same spindles; a scrub firing during a `btrfs replace` is kernel-rejected
#   and spuriously fires the scrub-failed alert. Unit tests pin the gate's
#   decisions in isolation, but the exit-4 contract only means anything through
#   real systemd: SuccessExitStatus must keep exit 4 off onFailure, and the
#   retry must now come from the timer's next poll -- there is no
#   RestartForceExitStatus, no RestartSec, and no durable deferred flag left to
#   carry it.
#
# Scenario: a 2-disk RAID1 pool whose owner paused a convert balance overnight,
#   with the scrub coming due.

import shlex

start_all()

passphrase = "testpassphrase"
pq = shlex.quote(passphrase)

SERVICE = "braid-scrub.service"
TIMER = "braid-scrub.timer"
SCRUB_FAILED_FLAG = "/var/lib/braid/scrub-failed"
ALERT_FIRED = "/root/alert-fired"


def add_cmd(key):
    return (
        f"printf '%s\\n' {pq} | braid add "
        f"--luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 "
        f"--luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 "
        f"{key}=/dev/disk/by-id/virtio-{key} --passphrase-stdin --yes"
    )


def show(node, unit, prop):
    return node.succeed("systemctl show {} -p {} --value".format(unit, prop)).strip()


def assert_no_alert(node, context):
    """A skip is not a failure: nothing on the alert path may have moved."""
    node.fail("test -f {}".format(SCRUB_FAILED_FLAG))
    node.fail("systemctl is-active braid-alert.service")
    node.fail("test -f {}".format(ALERT_FIRED))
    node.fail("systemctl is-failed {}".format(SERVICE))
    result = show(node, SERVICE, "Result")
    assert result == "success", "{}: expected Result=success, got {}".format(
        context, result
    )


def wait_for_exit(node, code, timeout=120):
    node.wait_until_succeeds(
        'test "$(systemctl show {} -p ExecMainStatus --value)" = {} && '
        'test "$(systemctl show {} -p ActiveState --value)" = inactive'.format(
            SERVICE, code, SERVICE
        ),
        timeout=timeout,
    )


with subtest("build a 2-disk RAID1 pool"):
    busy.wait_for_unit("multi-user.target", timeout=120)
    # Take the poll timer out of the picture before the pool ever comes online.
    # Every scrub run this test observes must come from an explicit start or
    # from a poll the test itself schedules -- an OnActiveSec poke firing on
    # activation would make those indistinguishable.
    busy.succeed("systemctl mask --runtime {}".format(TIMER))
    busy.succeed(add_cmd("disk1"))
    busy.succeed(add_cmd("disk2"))
    busy.succeed("mountpoint -q /mnt/storage")
    busy.succeed("dd if=/dev/urandom of=/mnt/storage/bigfile bs=1M count=512")
    busy.succeed("sync")

with subtest("a scrub due during a paused balance skips without scrubbing"):
    pause_balance_with_remaining_work(busy)
    busy.succeed("rm -f {} {}".format(SCRUB_FAILED_FLAG, ALERT_FIRED))
    # Type=simple: `systemctl start` returns as soon as the child forks, and the
    # skip exits almost immediately, so wait for the recorded exit code.
    busy.execute("systemctl start {}".format(SERVICE))
    wait_for_exit(busy, 4, timeout=60)
    # No scrub was started at all -- btrfs has never scrubbed this pool.
    busy.succeed(
        "btrfs scrub status --raw /mnt/storage | grep -q 'no stats available'"
    )
    busy.succeed("journalctl -u {} | grep -q 'scrub skipped'".format(SERVICE))

with subtest("the skip raises no alert and leaves no debt on disk"):
    assert_no_alert(busy, "after a busy skip")
    # The skip is purely informational now: the next poll re-derives everything
    # from btrfs's own record, so nothing may be written to carry it forward.
    busy.fail("test -e /var/lib/braid/scrub-deferred")

with subtest("the next poll runs a real scrub once the balance is gone"):
    busy.succeed("btrfs balance cancel /mnt/storage")
    # Unmasking and starting the timer is the poll: OnActiveSec=30s fires the
    # service shortly after the timer becomes active. No unit-level restart is
    # involved -- there is none left to be involved.
    busy.succeed("systemctl unmask --runtime {}".format(TIMER))
    busy.succeed("systemctl start {}".format(TIMER))
    busy.wait_until_succeeds(
        "btrfs scrub status --raw /mnt/storage | grep -q 'finished'", timeout=180
    )
    wait_for_exit(busy, 0)
    assert_no_alert(busy, "after the poll that ran the scrub")

with subtest("a poll on the now-fresh pool is a no-op"):
    # The flagship consequence of freshness scheduling: the very next poll must
    # not re-scrub a pool btrfs just finished scrubbing. It must also cost
    # nothing -- no second scrub, no alert, and a journal line the operator can
    # find when asking "why didn't my scrub run?".
    before = busy.succeed(
        "btrfs scrub status --raw /mnt/storage | grep 'Scrub started:'"
    ).strip()
    busy.succeed("systemctl start {}".format(SERVICE))
    wait_for_exit(busy, 0)
    busy.succeed("journalctl -u {} | grep -q 'scrub not due'".format(SERVICE))
    busy.succeed(
        "journalctl -u {} | grep -q 'last scrub started/resumed'".format(SERVICE)
    )
    after = busy.succeed(
        "btrfs scrub status --raw /mnt/storage | grep 'Scrub started:'"
    ).strip()
    assert before == after, (
        "a fresh poll must not have re-scrubbed the pool: {} -> {}".format(
            before, after
        )
    )
    assert_no_alert(busy, "after a not-due poll")

busy.shutdown()
