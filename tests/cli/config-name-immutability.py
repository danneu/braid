# Test: config disk-name immutability
#
# What: Builds a pool and disk-map entries, then renames one disk name in config
# while keeping the same by-id path and runs a mutating command.
#
# Why: v1.0 forbids name rename/reassignment in mutating commands; they must
# fail fast before probing or making storage changes.
#
# Dependencies: braid add succeeds and writes disk-map entries.

import json

start_all()
machine.wait_for_unit("multi-user.target")

import shlex

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def add_cmd(key):
    passphrase_q = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {passphrase_q} | "
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid add {key}=/dev/disk/by-id/virtio-{key} --passphrase-stdin --yes"
    )


with subtest("Setup: build 2-disk pool and disk-map entries"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "/dev/mapper/braid-disk1" in fi_show, fi_show
    assert "/dev/mapper/braid-disk2" in fi_show, fi_show

    raw_map = machine.succeed("cat /var/lib/braid/disk-map.json")
    disk_map = json.loads(raw_map)
    assert "disk1" in disk_map["disks"], disk_map
    assert "disk2" in disk_map["disks"], disk_map

with subtest("Add with renamed name for same disk is rejected"):
    # Try to add the same physical disk (virtio-disk1) under a new name (wd-red).
    # This should be rejected because disk-map already has disk1 for that by_id.
    map_before = machine.succeed("cat /var/lib/braid/disk-map.json")
    pq = shlex.quote(passphrase)
    status, output = machine.execute(
        f"printf '%s\\n' {pq} | "
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid add wd-red=/dev/disk/by-id/virtio-disk1 --passphrase-stdin --yes 2>&1"
    )
    assert status != 0, f"expected non-zero exit, got {status}:\n{output}"

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "/dev/mapper/braid-disk1" in fi_show, fi_show
    assert "/dev/mapper/braid-disk2" in fi_show, fi_show
    assert "missing" not in fi_show.lower(), fi_show

    map_after = machine.succeed("cat /var/lib/braid/disk-map.json")
    assert map_after == map_before, "disk-map changed on rejected name rename"

machine.shutdown()
