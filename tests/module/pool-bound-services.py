# Test: pool-bound-services
#
# Intent: braid.poolBoundServices stamps the pool lifecycle contract onto a
#   named long-running service while preserving that service's existing boot
#   edge.
#
# Why it exists: SMB/NFS consumers need all four fields from ADR 018. Without
#   WantedBy they do not restart after unlock; without After they lack stop
#   ordering before braid-online ExecStop; without ConditionPathIsMountPoint
#   they can serve the offline mountpoint.
#
# Scenario: A fake consumer service only declares its normal multi-user.target
#   boot edge and an ExecStart that holds fd 3 under /mnt/storage. The option
#   should skip it while locked, start it after unlock, stop it before lock
#   unmounts, and restart it on the next unlock.

import shlex

start_all()

passphrase = "testpassphrase"
pq = shlex.quote(passphrase)

CONSUMER = "dummy-pool-consumer.service"


def unlock(node):
    node.succeed("printf '%s\\n' {} | braid unlock --passphrase-stdin".format(pq))


def assert_consumer_holds_mount(node):
    node.succeed("systemctl is-active {}".format(CONSUMER))
    pid = node.succeed(
        "systemctl show -P MainPID {}".format(CONSUMER)
    ).strip()
    assert pid != "0" and pid != "", (
        CONSUMER + " has no MainPID: " + repr(pid)
    )
    target = node.succeed("readlink /proc/{}/fd/3".format(pid)).strip()
    assert target.startswith("/mnt/storage/"), (
        "consumer fd 3 does not point under /mnt/storage: " + repr(target)
    )


machine.wait_for_unit("multi-user.target", timeout=120)

with subtest("boot: consumer is condition-skipped while the pool is locked"):
    machine.fail("systemctl is-active {}".format(CONSUMER))
    machine.fail("mountpoint -q /mnt/storage")

with subtest("unit stamp: lifecycle edges merge with the existing boot edge"):
    wanted_by = machine.succeed(
        "systemctl show -P WantedBy {}".format(CONSUMER)
    ).strip()
    wanted_by_units = wanted_by.split()
    assert "multi-user.target" in wanted_by_units, (
        "WantedBy missing existing boot edge: " + repr(wanted_by)
    )
    assert "braid-online.service" in wanted_by_units, (
        "WantedBy missing braid-online.service: " + repr(wanted_by)
    )
    after = machine.succeed("systemctl show -P After {}".format(CONSUMER)).strip()
    assert "braid-online.service" in after.split(), (
        "After missing braid-online.service: " + repr(after)
    )

with subtest("unlock starts the consumer"):
    unlock(machine)
    machine.succeed("systemctl is-active braid-online.service")
    machine.wait_until_succeeds(
        "systemctl is-active {}".format(CONSUMER), timeout=30
    )
    assert_consumer_holds_mount(machine)

with subtest("BoundBy lists the stamped consumer"):
    bound_by = machine.succeed(
        "systemctl show -P BoundBy braid-online.service"
    ).strip()
    assert CONSUMER in bound_by.split(), (
        "BoundBy braid-online.service missing " + CONSUMER + ": " + repr(bound_by)
    )

with subtest("braid lock stops the consumer before unmount"):
    machine.succeed("braid lock")
    machine.fail("systemctl is-active {}".format(CONSUMER))
    machine.fail("mountpoint -q /mnt/storage")
    machine.fail("test -e /dev/mapper/braid-disk1")
    machine.fail("test -e /dev/mapper/braid-disk2")

with subtest("manual start while locked returns success but stays inactive"):
    machine.succeed("systemctl start {}".format(CONSUMER))
    machine.fail("systemctl is-active {}".format(CONSUMER))

with subtest("second unlock restarts the consumer"):
    unlock(machine)
    machine.succeed("systemctl is-active braid-online.service")
    machine.wait_until_succeeds(
        "systemctl is-active {}".format(CONSUMER), timeout=30
    )
    assert_consumer_holds_mount(machine)

machine.shutdown()
