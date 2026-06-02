# Test: recover from interrupted mixed-batch add
#
# Intent: `braid recover` on an Add::PoolMutation journal with one target
# already in btrfs must skip the live target in the replay loop, replay only
# the missing target, sweep ack entries for both target devids while leaving
# unrelated entries alone, rebuild pool.json, and run the post-add balance
# against real btrfs and LUKS.
#
# Why it exists: the mixed-batch recovery control flow is unit-tested, but
# the integration with live btrfs membership, real LUKS, acked-stats sweep,
# pool.json rebuild, and post-add balance handoff needs an end-to-end VM
# guard.
#
# Scenario: a 1-disk pool has disk1. `braid add disk2 disk3` commits disk2
# to btrfs, then crashes before disk3's pool_add_device, leaving pool.json
# with only disk1 and a pending Add journal for disk2 and disk3. Recovery must
# finish disk3 without reformatting or re-adding disk2.

import json
import re
import shlex
import uuid

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
pq = shlex.quote(passphrase)
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def add_cmd(name):
    return (
        f"printf '%s\\n' {pq} | "
        f"braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 {name}=/dev/disk/by-id/virtio-{name} --passphrase-stdin --yes"
    )


def read_json(path):
    return json.loads(machine.succeed(f"cat {path}"))


def write_json(path, value, marker):
    payload = json.dumps(value)
    machine.succeed(
        f"cat > {path} << '{marker}'\n"
        f"{payload}\n"
        f"{marker}"
    )


def member_entry(pool, name):
    for luks_uuid, member in pool["disks"].items():
        if member["name"] == name:
            return luks_uuid, member
    raise AssertionError(f"{name} missing from pool.json: {pool}")


def get_devid(mapper_name):
    fi_show = machine.succeed("btrfs filesystem show /mnt/storage")
    for line in fi_show.splitlines():
        if mapper_name in line:
            match = re.search(r"devid\s+(\d+)", line)
            if match:
                return int(match.group(1))
    raise AssertionError(f"devid not found for {mapper_name} in:\n{fi_show}")


def all_devids():
    fi_show = machine.succeed("btrfs filesystem show /mnt/storage")
    found = []
    for line in fi_show.splitlines():
        match = re.search(r"devid\s+(\d+)", line)
        if match:
            found.append(int(match.group(1)))
    assert found, f"expected btrfs devids in:\n{fi_show}"
    return found


def acked_entry(seed):
    return {
        "missing_acked": False,
        "device_stats": {
            "read_io_errs": seed,
            "write_io_errs": seed + 1,
            "flush_io_errs": seed + 2,
            "corruption_errs": seed + 3,
            "generation_errs": seed + 4,
        },
    }


# --- Phase 1: Build the base pool and write data ---

with subtest("Build 1-disk pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed("mountpoint -q /mnt/storage")
    machine.succeed("echo 'recover-add-mixed-batch-data' > /mnt/storage/testfile.txt")
    machine.succeed("sync")

    pool_json_after_disk1 = read_json("/var/lib/braid/pool.json")
    disk1_uuid, disk1_member = member_entry(pool_json_after_disk1, "disk1")


# --- Phase 2: Commit disk2 to btrfs without updating pool.json ---

with subtest("Prepare disk2 as committed but not bookkept"):
    disk2_uuid = str(uuid.uuid4())

    # Use printf '%s' (no newline) to match how braid passes the passphrase
    # to cryptsetup --key-file=- (braid strips the trailing newline).
    machine.succeed(
        f"printf '%s' {pq} | "
        f"cryptsetup luksFormat --batch-mode --key-file=- {luks_opts} "
        f"--uuid {disk2_uuid} --label braid-disk2 /dev/disk/by-id/virtio-disk2"
    )
    machine.succeed(
        f"printf '%s' {pq} | "
        "cryptsetup open --key-file=- /dev/disk/by-id/virtio-disk2 braid-disk2"
    )
    machine.succeed("btrfs device add /dev/mapper/braid-disk2 /mnt/storage")
    machine.succeed("sync")

    disk2_devid_before = get_devid("braid-disk2")
    disk2_luks_uuid_before = machine.succeed(
        "cryptsetup luksUUID /dev/disk/by-id/virtio-disk2"
    ).strip()
    assert disk2_luks_uuid_before == disk2_uuid, (
        f"seeded disk2 UUID mismatch: expected {disk2_uuid}, got {disk2_luks_uuid_before}"
    )

    live_devids = all_devids()
    disk3_expected_devid = max(live_devids) + 1
    disk3_uuid = str(uuid.uuid4())


# --- Phase 3: Seed acked-stats and inject the Add journal ---

with subtest("Inject mixed-batch add journal"):
    acked_stats = {
        str(disk2_devid_before): acked_entry(20),
        str(disk3_expected_devid): acked_entry(30),
        "99": acked_entry(99),
    }
    write_json("/var/lib/braid/acked-stats.json", acked_stats, "ACKED_EOF")

    journal = {
        "started_at": "2026-01-01T00:00:00Z",
        "op": {
            "op": "Add",
            "phase": "PoolMutation",
            "targets": {
                disk2_uuid: {
                    "name": "disk2",
                    "by_id": "/dev/disk/by-id/virtio-disk2",
                    "mode": {
                        "FreshLuks": {
                            "extra_opts": [
                                "--pbkdf",
                                "pbkdf2",
                                "--pbkdf-force-iterations",
                                "1000",
                            ],
                            "enroll_key_file": None,
                        }
                    },
                },
                disk3_uuid: {
                    "name": "disk3",
                    "by_id": "/dev/disk/by-id/virtio-disk3",
                    "mode": {
                        "FreshLuks": {
                            "extra_opts": [
                                "--pbkdf",
                                "pbkdf2",
                                "--pbkdf-force-iterations",
                                "1000",
                            ],
                            "enroll_key_file": None,
                        }
                    },
                },
            },
        },
        "pre_membership": pool_json_after_disk1,
        "target_membership": {
            "disks": {
                disk1_uuid: {
                    "name": disk1_member["name"],
                    "by_id": disk1_member["by_id"],
                },
                disk2_uuid: {
                    "name": "disk2",
                    "by_id": "/dev/disk/by-id/virtio-disk2",
                },
                disk3_uuid: {
                    "name": "disk3",
                    "by_id": "/dev/disk/by-id/virtio-disk3",
                },
            }
        },
    }
    write_json("/var/lib/braid/pending-op.json", journal, "JOURNAL_EOF")
    machine.succeed("test -f /var/lib/braid/pending-op.json")

    stale_pool = read_json("/var/lib/braid/pool.json")
    assert list(stale_pool["disks"].keys()) == [disk1_uuid], (
        f"pool.json should still contain only disk1 before recover: {stale_pool}"
    )


# --- Phase 4: Recover ---

with subtest("Recover mixed-batch add"):
    machine.succeed(
        f"printf '%s\\n' {pq} | braid recover --passphrase-stdin "
        ">/tmp/recover.out 2>/tmp/recover.err"
    )
    err = machine.succeed("cat /tmp/recover.err")

    banner = 'Recovering from interrupted "add" operation (started '
    assert banner in err, "recover banner line missing, got: " + repr(err)

    soft_replay_wait = "replaying post-add RAID1 soft balance"
    soft_replay_ok = "[ok]   pool: RAID1 soft balance replay complete\n"
    assert soft_replay_wait in err, (
        f"post-add soft balance replay wait row missing, got: {err!r}"
    )
    assert err.find(soft_replay_wait) < err.find(soft_replay_ok), (
        f"soft balance replay wait must precede ok row, got: {err!r}"
    )
    assert "recover remount cycle" not in err, (
        f"add recovery must not run the replace remount cycle, got: {err!r}"
    )

    completed_line = "pool.json written from completed add membership.\n"
    committed_line = "pool.json written from committed add membership.\n"
    assert completed_line in err, (
        "completed-add pool.json line missing, got: " + repr(err)
    )
    assert committed_line in err, (
        "committed-add pool.json line missing, got: " + repr(err)
    )
    assert err.find(completed_line) < err.find(committed_line), (
        "completed-add line must precede committed-add line, got: " + repr(err)
    )

    cleared_line = "pending-op.json cleared. Recovery complete.\n"
    assert cleared_line in err, "journal-cleared line missing, got: " + repr(err)
    # Pin the doc's recover skeleton: committed membership, owed soft balance,
    # then journal clear. The environment-specific open/mount rows stay out of
    # the doc example and this ordering guard.
    assert (
        err.find(committed_line)
        < err.find(soft_replay_wait)
        < err.find(soft_replay_ok)
        < err.find(cleared_line)
    ), "expected committed -> soft-balance replay -> journal-cleared order, got: " + repr(err)

    for triple_line in ("pre-operation membership:", "recovered (live pool):"):
        assert triple_line not in err, (
            f"add recovery must not print generic-live-pool line {triple_line!r}, "
            f"got: {err!r}"
        )

    machine.succeed("mountpoint -q /mnt/storage")


# --- Phase 5: Assert recovered device state ---

with subtest("Recovered live pool has disk2 unchanged and disk3 added"):
    disk2_luks_uuid_after = machine.succeed(
        "cryptsetup luksUUID /dev/disk/by-id/virtio-disk2"
    ).strip()
    assert disk2_luks_uuid_after == disk2_luks_uuid_before, (
        "disk2 LUKS UUID changed during recovery: "
        f"before={disk2_luks_uuid_before}, after={disk2_luks_uuid_after}"
    )

    disk2_devid_after = get_devid("braid-disk2")
    assert disk2_devid_after == disk2_devid_before, (
        f"disk2 devid changed: before={disk2_devid_before}, after={disk2_devid_after}"
    )

    machine.succeed("cryptsetup isLuks /dev/disk/by-id/virtio-disk3")
    disk3_luks_uuid_after = machine.succeed(
        "cryptsetup luksUUID /dev/disk/by-id/virtio-disk3"
    ).strip()
    assert disk3_luks_uuid_after == disk3_uuid, (
        f"disk3 LUKS UUID mismatch: expected {disk3_uuid}, got {disk3_luks_uuid_after}"
    )

    fi_show = machine.succeed("btrfs filesystem show /mnt/storage")
    assert "/dev/mapper/braid-disk2" in fi_show, f"disk2 missing:\n{fi_show}"
    assert "/dev/mapper/braid-disk3" in fi_show, f"disk3 missing:\n{fi_show}"


with subtest("pool.json rebuilt with all three disks"):
    recovered = read_json("/var/lib/braid/pool.json")
    expected = {
        disk1_uuid: ("disk1", "/dev/disk/by-id/virtio-disk1", get_devid("braid-disk1")),
        disk2_uuid: ("disk2", "/dev/disk/by-id/virtio-disk2", disk2_devid_before),
        disk3_uuid: ("disk3", "/dev/disk/by-id/virtio-disk3", get_devid("braid-disk3")),
    }
    assert set(recovered["disks"].keys()) == set(expected.keys()), (
        f"unexpected recovered members: {recovered}"
    )
    for luks_uuid, (name, by_id, devid) in expected.items():
        member = recovered["disks"][luks_uuid]
        assert member["name"] == name, f"{luks_uuid} name mismatch: {member}"
        assert member["by_id"] == by_id, f"{name} by_id mismatch: {member}"
        assert member["devid"] == devid, f"{name} devid mismatch: {member}"


with subtest("acked-stats target ghosts swept precisely"):
    disk3_actual_devid = get_devid("braid-disk3")
    assert disk3_actual_devid == disk3_expected_devid, (
        f"disk3 expected devid seed was wrong: expected {disk3_expected_devid}, "
        f"got {disk3_actual_devid}"
    )

    acked = read_json("/var/lib/braid/acked-stats.json")
    assert str(disk2_devid_before) not in acked, (
        f"disk2 target ack should be swept: {acked}"
    )
    assert str(disk3_expected_devid) not in acked, (
        f"disk3 target ack should be swept: {acked}"
    )
    assert acked.get("99") == acked_stats["99"], (
        f"unrelated ack entry should survive: {acked}"
    )


with subtest("Journal cleared and data intact"):
    machine.fail("test -f /var/lib/braid/pending-op.json")
    content = machine.succeed("cat /mnt/storage/testfile.txt").strip()
    assert content == "recover-add-mixed-batch-data", (
        f"expected sentinel data after recover, got: {content}"
    )


# --- Phase 6: Normal operations resume ---

with subtest("Normal lock and unlock preserve data"):
    machine.succeed("braid lock")
    machine.fail("mountpoint -q /mnt/storage")

    machine.succeed(f"printf '%s\\n' {pq} | braid unlock --passphrase-stdin")
    machine.succeed("mountpoint -q /mnt/storage")

    content = machine.succeed("cat /mnt/storage/testfile.txt").strip()
    assert content == "recover-add-mixed-batch-data", (
        f"expected sentinel data after unlock, got: {content}"
    )

machine.shutdown()
