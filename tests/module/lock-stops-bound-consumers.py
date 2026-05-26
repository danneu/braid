# Test: lock-stops-bound-consumers
#
# Intent: braid lock (user-initiated) and `systemctl stop braid-online.service`
#   (shutdown / manual ExecStop) both stop bound pool consumers and unmount
#   cleanly when a long-running consumer holds /mnt/storage busy, including a
#   BindsTo-only consumer with no stop-ordering guarantee.
#
# Why it exists: regression guard for the EBUSY-on-busy-mount class of bug
#   (samba on caja, future nfs/syncthing). The user-initiated lock path
#   relies on cmd_lock iterating BoundBy braid-online.service through
#   OnlineStateOps::list_bound_by; the ExecStop path relies on systemd's
#   BindsTo cascade stopping full-triad consumers before cmd_lock runs, and on
#   cmd_lock's explicit BoundBy stop for BindsTo-only consumers that may still
#   be active.
#
# Scenario: pool unlocked with a fake consumer service (dummy-pool-consumer)
#   holding fd 3 on /mnt/storage/.consumer-lock. Cycle 1 runs `braid lock`,
#   asserts teardown. Cycle 2 unlocks again, runs `systemctl stop
#   braid-online.service`, asserts teardown via ExecStop reentry. Cycle 3
#   manually starts a SIGTERM-resistant BindsTo-only consumer holding
#   /mnt/storage/.consumer-unordered-lock, then stops braid-online.service and
#   asserts clean teardown.

import shlex

start_all()

passphrase = "testpassphrase"
pq = shlex.quote(passphrase)

CONSUMER = "dummy-pool-consumer.service"
UNORDERED_CONSUMER = "dummy-pool-consumer-unordered.service"


def unlock(node):
    node.succeed("printf '%s\\n' {} | braid unlock --passphrase-stdin".format(pq))


def assert_consumer_holds_mount(node, unit):
    """Verify the consumer is active AND its fd 3 resolves under /mnt/storage.

    The fd check proves the consumer is genuinely holding the mount busy
    without depending on `fuser`/`lsof` being on PATH.
    """
    node.succeed("systemctl is-active {}".format(unit))
    pid = node.succeed(
        "systemctl show -P MainPID {}".format(unit)
    ).strip()
    assert pid != "0" and pid != "", (
        unit + " has no MainPID: " + repr(pid)
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
    assert_consumer_holds_mount(machine, CONSUMER)

with subtest("setup: BoundBy lists the consumer"):
    # Behavior-locks the BoundBy property name and shape against systemd
    # version drift -- cmd_lock depends on this.
    bound_by = machine.succeed(
        "systemctl show -P BoundBy braid-online.service"
    ).strip()
    assert CONSUMER in bound_by.split(), (
        "BoundBy braid-online.service missing " + CONSUMER + ": " + repr(bound_by)
    )

# === cycle 1: user-initiated braid lock ===

with subtest("cycle 1: braid lock stops consumer and unmounts"):
    machine.succeed("braid lock")
    # Consumer must be inactive after lock (cmd_lock's BoundBy pre-step stopped it).
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
    assert_consumer_holds_mount(machine, CONSUMER)

with subtest("cycle 2: systemctl stop braid-online.service unmounts via ExecStop"):
    # Exercises ExecStop reentry after systemd's BoundBy cascade:
    # systemd deactivates the consumer first, then ExecStop runs
    # `braid lock --systemd-stop`, whose cmd_lock BoundBy pre-step sees
    # the consumer already inactive (no-op stop). The systemd-stop arm
    # skips mark_offline, avoiding a recursive stop of the unit we are
    # already stopping.
    machine.succeed("systemctl stop braid-online.service")
    machine.fail("systemctl is-active {}".format(CONSUMER))
    machine.fail("mountpoint -q /mnt/storage")
    machine.fail("test -e /dev/mapper/braid-disk1")
    machine.fail("test -e /dev/mapper/braid-disk2")

# === cycle 3: ExecStop reentry with a BindsTo-only consumer ===

with subtest("cycle 3: re-unlock and start unordered consumer"):
    unlock(machine)
    machine.succeed("systemctl is-active braid-online.service")
    machine.wait_until_succeeds(
        "systemctl is-active {}".format(CONSUMER), timeout=30
    )
    assert_consumer_holds_mount(machine, CONSUMER)
    machine.succeed("systemctl start {}".format(UNORDERED_CONSUMER))
    assert_consumer_holds_mount(machine, UNORDERED_CONSUMER)

with subtest("cycle 3: BoundBy lists the unordered consumer"):
    bound_by = machine.succeed(
        "systemctl show -P BoundBy braid-online.service"
    ).strip()
    assert UNORDERED_CONSUMER in bound_by.split(), (
        "BoundBy braid-online.service missing "
        + UNORDERED_CONSUMER
        + ": "
        + repr(bound_by)
    )

with subtest("cycle 3: ExecStop stops unordered consumer before unmount"):
    machine.succeed("systemctl stop braid-online.service")
    machine.fail("systemctl is-active {}".format(CONSUMER))
    machine.fail("systemctl is-active {}".format(UNORDERED_CONSUMER))
    machine.fail("mountpoint -q /mnt/storage")
    machine.fail("test -e /dev/mapper/braid-disk1")
    machine.fail("test -e /dev/mapper/braid-disk2")

machine.shutdown()
