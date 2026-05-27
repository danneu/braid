# Test: status-mapper-drift
#
# Intent: Verify `braid status` renders a member's operator name through the
#   LUKS UUID join even when its live mapper has drifted.
#
# Why it exists: Status is the user-visible surface for UUID identity. A
#   regression that reconstructs names from mapper basenames could leak
#   `braid-WRONG` or `WRONG` instead of the pool.json disk name.
#
# Scenario: A two-disk pool is locked, then disk1 is manually reopened as
#   `braid-WRONG` while disk2 uses the normal mapper. The pool is mounted from
#   the drifted mapper, and status must still name disk1 by membership UUID.

import json
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
    uuid1 = machine.succeed(
        "cryptsetup luksUUID /dev/disk/by-id/virtio-disk1"
    ).strip()
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

with subtest("Status resolves the drifted member by UUID"):
    s = json.loads(machine.succeed("braid status --json"))
    assert s["status"] == "intact", s["status"]
    d1 = next((d for d in s["disks"] if d["luks_uuid"] == uuid1), None)
    assert d1 is not None, f"disk1 real UUID {uuid1} missing from status: {s}"
    assert d1["name"] == "disk1", (
        f"drifted mapper must still resolve to operator name, got {d1['name']!r}"
    )
    assert d1["mapper"] == "braid-WRONG", (
        f"expected observed mapper, got {d1['mapper']!r}"
    )
    assert d1["status"] == "present", d1
    d1_devid = d1["devid"]

    human = machine.succeed("braid status")
    compact_rows = [
        line
        for line in human.splitlines()
        if "present" in line and f"devid={d1_devid}" in line
    ]
    assert len(compact_rows) == 1, f"expected one drifted compact row:\n{human}"
    compact_name = compact_rows[0].split()[0]
    assert compact_name == "disk1", (
        f"drifted compact row must render operator name disk1, "
        f"got {compact_name!r}:\n{human}"
    )
    assert compact_name != "braid-WRONG", (
        f"mapper basename leaked into compact row:\n{human}"
    )

    detail_rows = [
        line
        for line in human.splitlines()
        if "present" in line and f"devid {d1_devid}" in line
    ]
    assert len(detail_rows) == 1, f"expected one drifted detail row:\n{human}"
    detail_name = detail_rows[0].split()[0]
    assert detail_name == "disk1", (
        f"drifted detail row must render operator name disk1, "
        f"got {detail_name!r}:\n{human}"
    )
    assert detail_name != "braid-WRONG", (
        f"mapper basename leaked into detail row:\n{human}"
    )

machine.shutdown()
