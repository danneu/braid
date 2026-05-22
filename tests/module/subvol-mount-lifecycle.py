# Test: subvol-mount-lifecycle
#
# Intent: A documented `systemd.mounts` subvolume mount participates in the
#   `braid lock` BoundBy cascade and starts again on the next unlock.
#
# Why it exists: manual/guides/mounting-subvolumes.md tells users to bind the
#   mount to braid-online.service and bind Jellyfin-style services to the
#   mount unit. This is the regression gate for that lifecycle shape.
#
# Scenario: The pool contains a `movies` subvolume mounted read-only at
#   /var/lib/jellyfin/media. A fake Jellyfin service holds a read-only fd inside
#   that mount. `braid lock` must stop the service and mount before LUKS close,
#   then `braid unlock` must bring both units back.

import shlex

start_all()

passphrase = "testpassphrase"
pq = shlex.quote(passphrase)

MOUNT = "var-lib-jellyfin-media.mount"
SERVICE = "dummy-jellyfin.service"


def unlock(node):
    node.succeed("printf '%s\\n' {} | braid unlock --passphrase-stdin".format(pq))


def assert_mapper_inactive(node, mapper):
    status, output = node.execute("cryptsetup status {} 2>&1".format(mapper))
    assert status != 0, "cryptsetup status unexpectedly succeeded for " + mapper
    assert "is inactive" in output, (
        "expected inactive cryptsetup status for " + mapper + ":\n" + output
    )


def assert_service_holds_subvol_mount(node):
    node.succeed("systemctl is-active {}".format(SERVICE))
    pid = node.succeed("systemctl show -P MainPID {}".format(SERVICE)).strip()
    assert pid != "0" and pid != "", (
        "dummy-jellyfin has no MainPID: " + repr(pid)
    )
    target = node.succeed("readlink /proc/{}/fd/3".format(pid)).strip()
    assert target.startswith("/var/lib/jellyfin/media/"), (
        "dummy-jellyfin fd 3 does not point under mounted media path: "
        + repr(target)
    )


machine.wait_for_unit("multi-user.target", timeout=120)

with subtest("cycle 1: unlock starts subvolume mount and bound service"):
    unlock(machine)
    machine.succeed("systemctl is-active braid-online.service")
    machine.wait_until_succeeds("systemctl is-active {}".format(MOUNT), timeout=30)
    machine.wait_until_succeeds("systemctl is-active {}".format(SERVICE), timeout=30)
    assert_service_holds_subvol_mount(machine)

with subtest("cycle 1: BoundBy lists the subvolume mount"):
    bound_by = machine.succeed(
        "systemctl show -P BoundBy braid-online.service"
    ).strip()
    assert MOUNT in bound_by.split(), (
        "BoundBy braid-online.service missing " + MOUNT + ": " + repr(bound_by)
    )

with subtest("cycle 1: braid lock stops bound service and mount"):
    machine.succeed("braid lock")
    machine.fail("systemctl is-active {}".format(SERVICE))
    machine.fail("systemctl is-active {}".format(MOUNT))
    machine.fail("mountpoint -q /var/lib/jellyfin/media")
    machine.fail("mountpoint -q /mnt/storage")
    assert_mapper_inactive(machine, "braid-disk1")
    assert_mapper_inactive(machine, "braid-disk2")

with subtest("cycle 2: unlock reactivates subvolume mount and service"):
    unlock(machine)
    machine.succeed("systemctl is-active braid-online.service")
    machine.wait_until_succeeds("systemctl is-active {}".format(MOUNT), timeout=30)
    machine.wait_until_succeeds("systemctl is-active {}".format(SERVICE), timeout=30)
    assert_service_holds_subvol_mount(machine)

machine.shutdown()
