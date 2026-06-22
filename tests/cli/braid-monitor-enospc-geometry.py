# Test: braid monitor - ENOSPC baseline invalidation after geometry change
#
# Intent: Verify that a keyed ENOSPC suppression baseline is discarded after a
#   same-devid `braid replace` changes the real btrfs device geometry.
#
# Why it exists: Unit tests cover PoolKey mismatches with hand-built keys. Only
#   a VM check proves the live `btrfs device usage --raw` probe, parser, and
#   PoolKey construction observe a real replace onto a larger disk and re-fire
#   a still-at-risk pool.
#
# Scenario: A skewed 3-disk RAID1 pool is at risk because one member is much
#   smaller than the others. After acknowledgment, disk1 is replaced with a
#   larger disk4. The FS UUID and devid set stay fixed, but the device_size for
#   disk1's preserved devid changes, so the stale baseline must not suppress the
#   next monitor run.

import json
import re
import shlex

start_all()
machine.wait_for_unit("multi-user.target", timeout=120)

passphrase = "testpassphrase"


def replace_cmd(old, new):
    passphrase_q = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {passphrase_q} | "
        "braid replace --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 "
        "--luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 "
        f"--old {old} --new {new}=/dev/disk/by-id/virtio-{new} "
        "--passphrase-stdin --yes"
    )


def monitor_exit():
    return machine.succeed("set +e; braid monitor; echo $?").strip().splitlines()[-1]


def btrfs_show():
    return machine.succeed("btrfs fi show /mnt/storage")


def get_fs_uuid():
    fi_show = btrfs_show()
    match = re.search(r"uuid:\s+([0-9a-fA-F-]+)", fi_show)
    assert match, f"FS UUID not found in:\n{fi_show}"
    return match.group(1)


def get_devid(mapper_name):
    fi_show = btrfs_show()
    for line in fi_show.splitlines():
        if mapper_name in line:
            match = re.search(r"devid\s+(\d+)", line)
            if match:
                return int(match.group(1))
    raise AssertionError(f"devid not found for {mapper_name} in:\n{fi_show}")


def get_devids():
    fi_show = btrfs_show()
    return sorted(int(devid) for devid in re.findall(r"\bdevid\s+(\d+)", fi_show))


def get_device_size_bytes(devid):
    usage = machine.succeed("btrfs device usage --raw /mnt/storage")
    current_devid = None
    for line in usage.splitlines():
        header = re.search(r", ID:\s+(\d+)$", line)
        if header:
            current_devid = int(header.group(1))
            continue
        if current_devid == devid:
            match = re.match(r"\s*Device size:\s+(\d+)$", line)
            if match:
                return int(match.group(1))
    raise AssertionError(f"Device size not found for devid {devid} in:\n{usage}")


with subtest("Monitor timer is active, then stopped for deterministic driving"):
    machine.succeed("systemctl is-active braid-monitor.timer")
    machine.succeed("systemctl stop braid-monitor.timer")

with subtest("Unlock pool via braid-pool.target"):
    machine.succeed("systemctl start braid-pool.target")
    machine.succeed("mountpoint -q /mnt/storage")

with subtest("Skewed pool starts below the ENOSPC threshold"):
    # The 512 MiB member is already below the threshold induced by the two
    # 4096 MiB members. That keeps the pool at risk without starving the large
    # source disk that btrfs replace needs to scrub.
    print(machine.succeed("btrfs device usage --raw /mnt/storage"))
    assert "ENOSPC risk" in machine.succeed("braid status"), (
        "skewed fixture did not start below the ENOSPC threshold"
    )

with subtest("At-risk pool latches EnospcRisk before ack"):
    rc = monitor_exit()
    assert rc == "3", f"Expected exit 3 (Warning), got {rc}"
    machine.succeed("test -f /var/lib/braid/alert-latch.json")

with subtest("Ack writes the keyed baseline for geometry G1"):
    machine.succeed("braid ack")
    machine.fail("test -f /var/lib/braid/alert-latch.json")
    machine.succeed("test -f /var/lib/braid/enospc-ack.json")

with subtest("Acked-but-unchanged pool stays suppressed"):
    rc = monitor_exit()
    assert rc == "0", f"Expected exit 0 (suppressed by baseline), got {rc}"
    machine.fail("test -f /var/lib/braid/alert-latch.json")

with subtest("Replace disk1 with larger disk4"):
    pre_uuid = get_fs_uuid()
    pre_devids = get_devids()
    disk1_devid = get_devid("braid-disk1")
    old_size = get_device_size_bytes(disk1_devid)

    result = machine.succeed(replace_cmd("disk1", "disk4"))
    print(f"braid replace output:\n{result}")

    fi_show = btrfs_show()
    print(f"Pool after replace:\n{fi_show}")
    assert "/dev/mapper/braid-disk4" in fi_show, (
        f"braid-disk4 missing from pool:\n{fi_show}"
    )

with subtest("Replace changed only the device_size axis"):
    assert get_fs_uuid() == pre_uuid, "FS UUID changed across live replace"
    assert get_devids() == pre_devids, (
        f"devid set changed from {pre_devids} to {get_devids()}"
    )

    disk4_devid = get_devid("braid-disk4")
    new_size = get_device_size_bytes(disk4_devid)
    print(
        f"disk4 devid {disk4_devid} size {new_size} bytes "
        f"(old disk1 devid {disk1_devid} size {old_size} bytes)"
    )

    assert disk4_devid == disk1_devid, (
        f"devid changed: disk1 had {disk1_devid}, disk4 has {disk4_devid}"
    )
    ratio = new_size / old_size
    assert ratio > 1.5, (
        "device_size should grow significantly after replace; "
        f"got {new_size} vs {old_size} bytes (ratio {ratio:.2f}x)"
    )

with subtest("Pool is still at ENOSPC risk after the geometry change"):
    status = machine.succeed("braid status")
    assert "ENOSPC risk" in status, f"expected ENOSPC risk after replace:\n{status}"

with subtest("Geometry change discards the baseline and re-fires"):
    rc = monitor_exit()
    assert rc == "3", f"Expected exit 3 after geometry change, got {rc}"
    machine.fail("test -f /var/lib/braid/enospc-ack.json")
    machine.succeed("test -f /var/lib/braid/alert-latch.json")

    report = json.loads(machine.succeed("braid status --json"))
    cause_types = [c["type"] for c in report["alert_causes"]]
    assert "enospc_risk" in cause_types, f"expected enospc_risk, got {cause_types}"

machine.shutdown()
