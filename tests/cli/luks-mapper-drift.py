# Test: luks-mapper-drift
#
# Intent: Verify `braid lock` closes the observed live mapper for a member
#   even when it does not match the mapper name derived from pool.json.
#
# Why it exists: LUKS UUID is the member identity. A lock implementation that
#   reconstructs `braid-<name>` from membership could leave a drifted but
#   member-owned mapper open, making "locked" state false.
#
# Scenario: A pool member is manually opened as `braid-WRONG`, mounted, and
#   then locked. Braid must close `braid-WRONG`, not just the expected
#   `braid-disk1` name.

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


def unlock_cmd():
    return f"printf '%s\\n' {pq} | braid unlock --passphrase-stdin"


with subtest("Build and lock a two-disk pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed("mountpoint -q /mnt/storage")
    machine.succeed("braid lock")
    machine.fail("mountpoint -q /mnt/storage")
    machine.fail("test -e /dev/mapper/braid-disk1")
    machine.fail("test -e /dev/mapper/braid-disk2")

with subtest("Manually mount disk1 under a drifted mapper"):
    machine.succeed(
        f"printf '%s' {pq} | cryptsetup open --key-file=- "
        "/dev/disk/by-id/virtio-disk1 braid-WRONG"
    )
    machine.succeed(
        f"printf '%s' {pq} | cryptsetup open --key-file=- "
        "/dev/disk/by-id/virtio-disk2 braid-disk2"
    )
    machine.succeed("btrfs device scan")
    machine.succeed(
        "mount -o noatime,skip_balance,subvolid=5 "
        "/dev/mapper/braid-WRONG /mnt/storage"
    )
    fi_show = machine.succeed("btrfs filesystem show /mnt/storage")
    assert "/dev/mapper/braid-WRONG" in fi_show, fi_show
    assert "/dev/mapper/braid-disk2" in fi_show, fi_show
    assert "/dev/mapper/braid-disk1" not in fi_show, fi_show

with subtest("Lock closes the observed drifted mapper"):
    status, output = machine.execute("braid lock 2>&1")
    assert status == 0, f"braid lock failed: {output}"
    assert "disk disk1: locking" in output, output
    assert "disk disk1: locked" in output, output
    machine.fail("mountpoint -q /mnt/storage")
    machine.fail("test -e /dev/mapper/braid-WRONG")
    machine.fail("test -e /dev/mapper/braid-disk1")
    machine.fail("test -e /dev/mapper/braid-disk2")

with subtest("Unlock restores the expected mapper names"):
    machine.succeed(unlock_cmd())
    machine.succeed("mountpoint -q /mnt/storage")
    machine.succeed("test -e /dev/mapper/braid-disk1")
    machine.succeed("test -e /dev/mapper/braid-disk2")
    machine.fail("test -e /dev/mapper/braid-WRONG")

machine.shutdown()
