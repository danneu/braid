# Test: scrub-lifecycle
#
# Intent: Verify the two key behaviors of lifecycle-bound scrub: (1) Persistent
#   catch-up fires immediately after pool unlock when the timer stamp is overdue,
#   and (2) braid lock succeeds while the scrub service is actively holding the
#   mount busy, because the wrapper stops the timer and service first.
#
# Why it exists: Config tests verify unit properties (BindsTo, Persistent, etc.)
#   but only a behavioral test proves the catch-up actually fires and the
#   cancellation path works end-to-end. These are the two behaviors that justify
#   owning the scrub timer instead of delegating to services.btrfs.autoScrub.
#
# Scenario: Two nodes, each with a 2-disk RAID1 pool (initrd fixture).
#   catchup: real scrub service, seeded overdue stamp, unlock triggers catch-up.
#   cancel:  fake long-running scrub (holds mount busy), lock succeeds because
#            wrapper stops timer+service before CLI unmounts.

import shlex

start_all()

passphrase = "testpassphrase"
pq = shlex.quote(passphrase)

TIMER = "braid-scrub.timer"
SERVICE = "braid-scrub.service"
STAMP = "/var/lib/systemd/timers/stamp-braid-scrub.timer"


def show(node, unit, prop):
    return node.succeed(
        "systemctl show {} -p {} --value".format(unit, prop)
    ).strip()


# === catchup node: Persistent catch-up ===

with subtest("catchup: timer inactive before unlock"):
    catchup.wait_for_unit("multi-user.target", timeout=120)
    catchup.succeed("systemctl cat {}".format(TIMER))
    catchup.fail("systemctl is-active {}".format(TIMER))

with subtest("catchup: Persistent catch-up fires on unlock with overdue stamp"):
    # Seed old stamp file to create explicit overdue state. This simulates a
    # timer that last fired on 2025-01-01 — well past the monthly boundary.
    catchup.succeed("mkdir -p /var/lib/systemd/timers")
    catchup.succeed("touch -t 202501010000 {}".format(STAMP))

    catchup.succeed(
        "printf '%s\\n' {} | braid unlock --passphrase-stdin".format(pq)
    )
    catchup.succeed("systemctl is-active braid-online.service")
    catchup.succeed("systemctl is-active {}".format(TIMER))

    # Persistent=true reads old stamp, fires immediately. Scrub on tiny disk
    # completes in milliseconds, so check Result (not ActiveState).
    catchup.wait_until_succeeds(
        "test \"$(systemctl show {} -p Result --value)\" = success".format(
            SERVICE
        ),
        timeout=30,
    )

with subtest("catchup: timer stops when pool is locked"):
    catchup.succeed("braid lock")
    catchup.fail("systemctl is-active {}".format(TIMER))
    catchup.fail("systemctl is-active braid-online.service")

with subtest("catchup: catch-up fires again after stamp is re-aged"):
    # Record monotonic timestamp from the previous scrub run.
    old_ts = show(catchup, SERVICE, "ExecMainStartTimestampMonotonic")

    # Age the stamp back to 2025-01-01. The timer is stopped (lock stopped
    # braid-online → BindsTo stopped the timer), so systemd won't interfere.
    catchup.succeed("touch -t 202501010000 {}".format(STAMP))

    catchup.succeed(
        "printf '%s\\n' {} | braid unlock --passphrase-stdin".format(pq)
    )

    # Wait for a NEW scrub activation (timestamp must differ from old run).
    catchup.wait_until_succeeds(
        "test \"$(systemctl show {} -p ExecMainStartTimestampMonotonic --value)\" != '{}'"
        " && test \"$(systemctl show {} -p Result --value)\" = success".format(
            SERVICE, old_ts, SERVICE
        ),
        timeout=30,
    )

catchup.shutdown()

# === cancel node: safe cancellation during lock ===

with subtest("cancel: lock succeeds while scrub holds mount busy"):
    cancel.wait_for_unit("multi-user.target", timeout=120)

    # Seed old stamp so Persistent fires the fake scrub immediately on unlock.
    cancel.succeed("mkdir -p /var/lib/systemd/timers")
    cancel.succeed("touch -t 202501010000 {}".format(STAMP))

    cancel.succeed(
        "printf '%s\\n' {} | braid unlock --passphrase-stdin".format(pq)
    )

    # Wait for fake scrub to be actively running (sleep 300 with open FD).
    cancel.wait_until_succeeds(
        "systemctl is-active {}".format(SERVICE)
    )

    # Lock while scrub service is actively holding the mount.
    # The wrapper must stop the timer and service before CLI attempts unmount.
    cancel.succeed("braid lock")
    cancel.fail("mountpoint -q /mnt/storage")
    cancel.fail("test -e /dev/mapper/braid-disk1")
    cancel.fail("test -e /dev/mapper/braid-disk2")

cancel.shutdown()
