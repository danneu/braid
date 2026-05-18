# Test: luks-lock-skipped-no-false-closed
#
# Intent: Verify `braid lock` does not print "already closed" for pool
#   members when an unverified braid-prefixed mapper is skipped.
#
# Why it exists: a skipped mapper can be a drifted member whose backing
#   LUKS UUID could not be read. Reporting every unplanned member as
#   already closed would contradict the planner's cleanup uncertainty.
#
# Scenario: A normal two-disk pool is fully locked, then a bogus
#   `/dev/mapper/braid-WRONG` entry appears. A second `braid lock` must warn
#   about the skipped mapper without claiming either member is closed.

import shlex

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
pq = shlex.quote(passphrase)


def add_cmd(name):
    return (
        f"printf '%s\\n' {pq} | "
        "braid add "
        "--luks-format-arg=--pbkdf "
        "--luks-format-arg=pbkdf2 "
        "--luks-format-arg=--pbkdf-force-iterations "
        "--luks-format-arg=1000 "
        f"{name}=/dev/disk/by-id/virtio-{name} "
        "--passphrase-stdin --yes"
    )


with subtest("Build and lock a two-disk pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed("mountpoint -q /mnt/storage")
    machine.succeed("braid lock")
    machine.fail("mountpoint -q /mnt/storage")
    machine.fail("test -e /dev/mapper/braid-disk1")
    machine.fail("test -e /dev/mapper/braid-disk2")

with subtest("Create an unclassifiable braid-prefixed mapper candidate"):
    machine.succeed("touch /dev/mapper/braid-WRONG")
    machine.succeed("test -e /dev/mapper/braid-WRONG")

with subtest("Lock warns about the skip without false closed rows"):
    status, output = machine.execute("braid lock 2>&1")
    assert status == 0, "braid lock failed: " + output
    assert "skipping mapper braid-WRONG" in output, output
    assert "disk disk1: already closed" not in output, output
    assert "disk disk2: already closed" not in output, output
    assert "pool already locked" not in output, output

machine.shutdown()
