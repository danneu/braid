# Test: lock-stops-bound-consumers
#
# Intent: braid lock (user-initiated) and `systemctl stop braid-online.service`
#   (shutdown / manual ExecStop) both stop bound pool consumers and unmount
#   cleanly when a long-running consumer holds /mnt/storage busy.
#
# Why it exists: regression guard for the EBUSY-on-busy-mount class of bug
#   (samba on caja, future nfs/syncthing). The user-initiated lock path
#   relies on the wrapper iterating BoundBy braid-online.service; the
#   ExecStop path relies on systemd's BindsTo cascade. Both paths run
#   through braid-wrapper.sh's pre-stop block on reentry, so both are tested.
#
# Scenario: pool unlocked with a fake consumer service (dummy-pool-consumer)
#   holding fd 3 on /mnt/storage/.consumer-lock. Cycle 1 runs `braid lock`,
#   asserts teardown. Cycle 2 unlocks again, runs `systemctl stop
#   braid-online.service`, asserts teardown via ExecStop reentry.

import shlex

start_all()

passphrase = "testpassphrase"
pq = shlex.quote(passphrase)

CONSUMER = "dummy-pool-consumer.service"


def unlock(node):
    node.succeed("printf '%s\\n' {} | braid unlock --passphrase-stdin".format(pq))


def assert_consumer_holds_mount(node):
    """Verify dummy-pool-consumer is active AND its fd 3 resolves under
    /mnt/storage. The fd check proves the consumer is genuinely holding the
    mount busy without depending on `fuser`/`lsof` being on PATH."""
    node.succeed("systemctl is-active {}".format(CONSUMER))
    pid = node.succeed(
        "systemctl show -P MainPID {}".format(CONSUMER)
    ).strip()
    assert pid != "0" and pid != "", (
        "dummy-pool-consumer has no MainPID: " + repr(pid)
    )
    target = node.succeed("readlink /proc/{}/fd/3".format(pid)).strip()
    assert target.startswith("/mnt/storage/"), (
        "consumer fd 3 does not point under /mnt/storage: " + repr(target)
    )


machine.wait_for_unit("multi-user.target", timeout=120)

# === setup: unlock and confirm consumer holds the mount ===

with subtest("setup: unlock pool and start consumer"):
    unlock(machine)
    machine.succeed("systemctl is-active braid-online.service")
    machine.wait_until_succeeds(
        "systemctl is-active {}".format(CONSUMER), timeout=30
    )
    assert_consumer_holds_mount(machine)

with subtest("setup: BoundBy lists the consumer"):
    # Behavior-locks the BoundBy property name and shape against systemd
    # version drift -- the wrapper depends on this.
    bound_by = machine.succeed(
        "systemctl show -P BoundBy braid-online.service"
    ).strip()
    assert CONSUMER in bound_by.split(), (
        "BoundBy braid-online.service missing " + CONSUMER + ": " + repr(bound_by)
    )

# === cycle 1: user-initiated braid lock ===

with subtest("cycle 1: braid lock stops consumer and unmounts"):
    machine.succeed("braid lock")
    # Consumer must be inactive after lock (wrapper's BoundBy loop stopped it).
    machine.fail("systemctl is-active {}".format(CONSUMER))
    machine.fail("mountpoint -q /mnt/storage")
    # `cryptsetup status` exits non-zero for inactive mappers; the test
    # driver's auto-prepended `set -euo pipefail` would abort the script.
    # Use the device-node check instead, matching scrub-lifecycle.py.
    machine.fail("test -e /dev/mapper/braid-disk1")
    machine.fail("test -e /dev/mapper/braid-disk2")

# === cycle 2: ExecStop reentry via systemctl stop braid-online.service ===

with subtest("cycle 2: re-unlock and confirm consumer holds the mount again"):
    unlock(machine)
    machine.succeed("systemctl is-active braid-online.service")
    machine.wait_until_succeeds(
        "systemctl is-active {}".format(CONSUMER), timeout=30
    )
    assert_consumer_holds_mount(machine)

with subtest("cycle 2: systemctl stop braid-online.service unmounts via ExecStop"):
    # Exercises ExecStop reentry through the wrapper's BoundBy loop:
    # systemd's BindsTo cascade deactivates the consumer first, then
    # ExecStop=braid lock runs the wrapper, whose loop sees the consumer
    # already inactive (no-op stop). The wrapper sees
    # BRAID_SYSTEMD_EXECSTOP=1 from braid-online's ExecStop and skips its
    # own recursive braid-online stop, avoiding a deadlock against the
    # in-progress stop we initiated here.
    machine.succeed("systemctl stop braid-online.service")
    machine.fail("systemctl is-active {}".format(CONSUMER))
    machine.fail("mountpoint -q /mnt/storage")
    machine.fail("test -e /dev/mapper/braid-disk1")
    machine.fail("test -e /dev/mapper/braid-disk2")

machine.shutdown()
