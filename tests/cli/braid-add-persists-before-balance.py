# Intent: pool.json is written before the post-add balance starts, so
# bookkeeping is consistent with the live btrfs pool while balance runs.
#
# Why it exists: an interrupted post-add balance used to leave pool.json
# reporting N disks while the live pool had N+1. `braid status` then
# disagreed with pool.json, pushing the operator into recovery just to
# reconcile bookkeeping.
#
# Scenario: bootstrap a 1-disk pool, write enough single-profile data to
# make the post-add balance observable, run `braid add disk2` in the
# background, and assert pool.json contains disk2 with enriched metadata
# while the balance is still running and pending-op.json still exists.

import json
import uuid


def member_names(pool):
    return {member["name"] for member in pool["disks"].values()}


def member(pool, name):
    for entry in pool["disks"].values():
        if entry["name"] == name:
            return entry
    raise AssertionError(f"{name} missing from pool.json: {pool}")
import re
import shlex
import time

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
pq = shlex.quote(passphrase)
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def add_cmd(key):
    return (
        f"printf '%s\\n' {pq} | "
        f"braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 {key}=/dev/disk/by-id/virtio-{key} --passphrase-stdin --yes"
    )


def add_cmd_bg(key):
    return (
        f"("
        f"printf '%s\\n' {pq} | "
        f"braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 {key}=/dev/disk/by-id/virtio-{key} --passphrase-stdin --yes; "
        f"echo $? > /tmp/add.exit"
        f") > /tmp/add.log 2>&1 & echo $! > /tmp/add.pid"
    )


with subtest("Build 1-disk pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed("mountpoint -q /mnt/storage")

with subtest("Write single-profile payload"):
    machine.succeed(
        "dd if=/dev/urandom of=/mnt/storage/payload bs=1M count=3000 status=none"
    )
    machine.succeed("sync")

with subtest("Pre-add chunks are single-profile"):
    fi_df = machine.succeed("btrfs filesystem df /mnt/storage")
    print(f"=== fi df pre-add ===\n{fi_df}")
    assert "Data, single" in fi_df, (
        f"expected single-profile data before second disk add:\n{fi_df}"
    )

PCT_RE = re.compile(r"(\d+)% left")

with subtest("Start braid add and wait for post-add balance"):
    machine.execute(add_cmd_bg("disk2"))

    saw_balance_with_room = False
    last_status = ""
    for _ in range(800):
        status_ret = machine.execute("btrfs balance status /mnt/storage 2>&1")
        last_status = status_ret[1]
        if "is running" in last_status:
            m = PCT_RE.search(last_status)
            if m and int(m.group(1)) >= 70:
                saw_balance_with_room = True
                print(f"balance status: {last_status.strip()}")
                break
        time.sleep(0.05)

    assert saw_balance_with_room, (
        "Never observed the post-add balance running with >=70% of work "
        "remaining. Last balance status:\n"
        f"{last_status}\n"
        "add log:\n"
        + machine.execute("cat /tmp/add.log 2>&1")[1]
    )

with subtest("pool.json already contains the new disk during balance"):
    pool_json = machine.succeed("cat /var/lib/braid/pool.json")
    print(f"=== pool.json during balance ===\n{pool_json}")
    membership = json.loads(pool_json)

    assert "disk1" in member_names(membership), f"disk1 missing from pool.json:\n{pool_json}"
    assert "disk2" in member_names(membership), f"disk2 missing from pool.json:\n{pool_json}"

    disk2_uuid = next(
        uuid
        for uuid, entry in membership["disks"].items()
        if entry["name"] == "disk2"
    )
    disk2 = member(membership, "disk2")
    assert disk2_uuid, f"disk2 UUID key missing:\n{pool_json}"
    assert str(uuid.UUID(disk2_uuid)) == disk2_uuid, (
        f"disk2 pool.json key is not canonical LUKS UUID form:\n{pool_json}"
    )
    assert "luks_uuid" not in disk2, (
        f"disk2 must not carry duplicate value-side luks_uuid:\n{pool_json}"
    )
    assert disk2.get("devid") is not None, f"disk2 devid missing:\n{pool_json}"
    assert disk2.get("added_at"), f"disk2 added_at missing:\n{pool_json}"

    machine.succeed("test -f /var/lib/braid/pending-op.json")

with subtest("braid add finishes and clears the journal"):
    for _ in range(1200):
        if machine.execute("test -f /tmp/add.exit")[0] == 0:
            break
        time.sleep(0.1)
    else:
        raise AssertionError(
            "braid add did not finish. add log:\n"
            + machine.execute("cat /tmp/add.log 2>&1")[1]
        )

    add_exit = int(machine.succeed("cat /tmp/add.exit").strip())
    add_log = machine.succeed("cat /tmp/add.log")
    print(f"=== braid add log (exit {add_exit}) ===\n{add_log}")
    assert add_exit == 0, f"braid add failed with exit {add_exit}:\n{add_log}"
    machine.fail("test -f /var/lib/braid/pending-op.json")

machine.shutdown()
