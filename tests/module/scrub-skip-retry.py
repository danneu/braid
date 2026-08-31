# Test: scrub-skip-retry
#
# Intent: Verify the busy-scrub gate end to end -- a scheduled scrub that fires
#   while a balance is in flight exits 4 without starting any scrub and without
#   touching scrub state; the skip raises no alert (no scrub-failed flag, no
#   beeper, no alertCommand), including when the retry wait is stopped; the
#   automatic retry runs the scrub for real once the pool is clear; and a
#   deferral left in /var/lib/braid makes the pool-online resume trigger start
#   the service again.
#
# Why it exists: on caja a `braid add` convert balance was mid-flight with the
#   monthly scrub due at midnight and nothing stopped the scrub from piling onto
#   the same spindles; a scrub firing during a `btrfs replace` is kernel-rejected
#   and spuriously fires the scrub-failed alert. Unit tests pin the gate's
#   decisions in isolation, but the exit-4 contract only means anything through
#   real systemd: SuccessExitStatus must keep exit 4 off onFailure while
#   RestartForceExitStatus still schedules the retry, and the durable deferred
#   flag must outlive a stopped retry.
#
# Scenario: a 2-disk RAID1 pool whose owner paused a convert balance overnight,
#   with the monthly scrub due.

import shlex

start_all()

passphrase = "testpassphrase"
pq = shlex.quote(passphrase)

SERVICE = "braid-scrub.service"
SCRUB_FAILED_FLAG = "/var/lib/braid/scrub-failed"
DEFERRED_FLAG = "/var/lib/braid/scrub-deferred"
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


with subtest("build a 2-disk RAID1 pool"):
    busy.wait_for_unit("multi-user.target", timeout=120)
    # Take the calendar timer out of the picture before the pool ever comes
    # online. Every scrub run this test observes must come from an explicit
    # start, the exit-4 retry, or the pool-online resume trigger -- a
    # Persistent=true monthly timer firing on activation would make those
    # indistinguishable.
    busy.succeed("systemctl mask --runtime braid-scrub.timer")
    busy.succeed(add_cmd("disk1"))
    busy.succeed(add_cmd("disk2"))
    busy.succeed("mountpoint -q /mnt/storage")
    busy.succeed("dd if=/dev/urandom of=/mnt/storage/bigfile bs=1M count=512")
    busy.succeed("sync")

with subtest("a scrub due during a paused balance skips without scrubbing"):
    pause_balance_with_remaining_work(busy)
    busy.succeed("rm -f {} {} {}".format(SCRUB_FAILED_FLAG, DEFERRED_FLAG, ALERT_FIRED))
    # Type=simple: `systemctl start` returns as soon as the child forks, and the
    # skip exits almost immediately, so wait for the recorded exit code.
    busy.execute("systemctl start {}".format(SERVICE))
    busy.wait_until_succeeds(
        'test "$(systemctl show {} -p ExecMainStatus --value)" = 4'.format(SERVICE),
        timeout=60,
    )
    # No scrub was started at all -- btrfs has never scrubbed this pool.
    busy.succeed(
        "btrfs scrub status --raw /mnt/storage | grep -q 'no stats available'"
    )
    busy.succeed("journalctl -u {} | grep -q 'scrub skipped'".format(SERVICE))

with subtest("the skip is recorded durably and raises no alert"):
    busy.succeed("test -f {}".format(DEFERRED_FLAG))
    assert_no_alert(busy, "after a busy skip")

with subtest("stopping the retry wait still raises no alert"):
    # The unit sits in auto-restart between retries. A `systemctl stop` there
    # (the shape of `sleep.target` or a shutdown arriving mid-wait) must not
    # turn the skip into a failure.
    busy.wait_until_succeeds(
        'test "$(systemctl show {} -p SubState --value)" = auto-restart'.format(
            SERVICE
        ),
        timeout=60,
    )
    busy.succeed("systemctl stop {}".format(SERVICE))
    assert_no_alert(busy, "after stopping the retry wait")
    busy.succeed("test -f {}".format(DEFERRED_FLAG))

with subtest("the retry runs a real scrub once the balance is gone"):
    busy.succeed("systemctl start {}".format(SERVICE))
    # Wait for the unit to actually be sitting in its retry wait, so the run
    # that follows the cancel is unambiguously the scheduled retry.
    busy.wait_until_succeeds(
        'test "$(systemctl show {} -p SubState --value)" = auto-restart'.format(
            SERVICE
        ),
        timeout=60,
    )
    busy.succeed("btrfs balance cancel /mnt/storage")
    # No timer fire is involved: RestartSec=5s alone must produce the run.
    busy.wait_until_succeeds(
        'test "$(systemctl show {} -p ExecMainStatus --value)" = 0'.format(SERVICE),
        timeout=120,
    )
    busy.succeed("btrfs scrub status --raw /mnt/storage | grep -q 'finished'")
    busy.fail("test -f {}".format(DEFERRED_FLAG))
    assert_no_alert(busy, "after the successful retry")

with subtest("a deferral at pool-online re-pokes the scrub service"):
    # Stands in for the reboot case: systemd's pending restart is gone, so the
    # durable flag plus the pool-online resume trigger is the only thing left
    # that owes the scrub a run.
    busy.succeed("systemctl stop {}".format(SERVICE))
    busy.succeed("touch {}".format(DEFERRED_FLAG))
    # The previous subtest already left ExecMainStatus=0, so pin the *new* run
    # by its invocation id rather than its exit code. InvocationID is set when
    # the unit *starts*, though, and ExecMainStatus still holds the stale 0
    # from the previous run at that moment -- so also require the unit to be
    # back at rest (Type=simple, so a successful run ends inactive). Without
    # that conjunct the wait returns while braid is still starting up and races
    # the clear_deferral below.
    previous_invocation = show(busy, SERVICE, "InvocationID")
    busy.succeed("braid lock")
    busy.succeed("printf '%s\\n' {} | braid unlock --passphrase-stdin".format(pq))
    busy.wait_until_succeeds(
        'test "$(systemctl show {} -p InvocationID --value)" != {} && '
        'test "$(systemctl show {} -p ActiveState --value)" = inactive && '
        'test "$(systemctl show {} -p ExecMainStatus --value)" = 0'.format(
            SERVICE, shlex.quote(previous_invocation), SERVICE, SERVICE
        ),
        timeout=120,
    )
    busy.fail("test -f {}".format(DEFERRED_FLAG))
    assert_no_alert(busy, "after the pool-online resume")

busy.shutdown()
