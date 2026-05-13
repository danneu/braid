# Test: config disk-name immutability
#
# What: Builds a pool and pool membership entries, then renames one disk name in config
# while keeping the same by-id path and runs a mutating command.
#
# Why: v1.0 forbids name rename/reassignment in mutating commands; they must
# fail fast before probing or making storage changes.
#
# Dependencies: braid add succeeds and writes pool membership entries.

import json


def member_names(pool):
    return {member["name"] for member in pool["disks"].values()}


def member(pool, name):
    for entry in pool["disks"].values():
        if entry["name"] == name:
            return entry
    raise AssertionError(f"{name} missing from pool.json: {pool}")

start_all()
machine.wait_for_unit("multi-user.target")

import shlex

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def add_cmd(key):
    passphrase_q = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {passphrase_q} | "
        f"braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 {key}=/dev/disk/by-id/virtio-{key} --passphrase-stdin --yes"
    )


with subtest("Setup: build 2-disk pool and pool membership"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "/dev/mapper/braid-disk1" in fi_show, fi_show
    assert "/dev/mapper/braid-disk2" in fi_show, fi_show

    raw_pool = machine.succeed("cat /var/lib/braid/pool.json")
    pool_m = json.loads(raw_pool)
    assert "disk1" in member_names(pool_m), pool_m
    assert "disk2" in member_names(pool_m), pool_m

with subtest("Add with renamed name for same disk is rejected"):
    # Try to add the same physical disk (virtio-disk1) under a new name (wd-red).
    # This should be rejected because pool.json already has disk1 for that by_id.
    pool_before = machine.succeed("cat /var/lib/braid/pool.json")
    pq = shlex.quote(passphrase)
    status, output = machine.execute(
        f"printf '%s\\n' {pq} | "
        f"braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 wd-red=/dev/disk/by-id/virtio-disk1 --passphrase-stdin --yes 2>&1"
    )
    assert status != 0, f"expected non-zero exit, got {status}:\n{output}"

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "/dev/mapper/braid-disk1" in fi_show, fi_show
    assert "/dev/mapper/braid-disk2" in fi_show, fi_show
    assert "missing" not in fi_show.lower(), fi_show

    pool_after = machine.succeed("cat /var/lib/braid/pool.json")
    assert pool_after == pool_before, "pool.json changed on rejected name rename"

machine.shutdown()
