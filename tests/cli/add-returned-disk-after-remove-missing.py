# Intent: prove a removed-missing braid-labeled disk can return through
# `braid add` without manual wiping.
#
# Why it exists: existing-pool add must accept a verified returned disk by
# replaying the narrow returned-disk path: open the old LUKS container, validate
# the stale btrfs FSID, forget/wipe only the btrfs signature, and force-add it
# back to the mounted pool. Unit tests pin crash windows; this VM test proves
# the real CLI path works with real btrfs and LUKS.
#
# Scenario: a 3-disk RAID1 pool loses disk3. The operator mounts degraded,
# removes the missing devid, then the same physical disk returns. Re-adding
# disk3 should succeed, preserve data, update pool.json, and leave no journal.

import json
import shlex

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def add_cmd(key):
    passphrase_q = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {passphrase_q} | "
        f"braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 {key}=/dev/disk/by-id/virtio-{key} --passphrase-stdin --yes"
    )


def missing_devid():
    raw = machine.succeed("braid status --json")
    report = json.loads(raw)
    devids = report.get("missing_devids", [])
    assert len(devids) == 1, f"expected one missing devid, got {devids}:\n{raw}"
    return str(devids[0])


with subtest("Build 3-disk RAID1 pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed(add_cmd("disk3"))
    machine.succeed("echo 'returned disk data' > /mnt/storage/kept.txt")
    machine.succeed("sync")
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    for name in ["braid-disk1", "braid-disk2", "braid-disk3"]:
        assert f"/dev/mapper/{name}" in fi_show, f"{name} missing:\n{fi_show}"


with subtest("Make disk3 missing and remove it from btrfs membership"):
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup close braid-disk3")
    machine.succeed("mount -o degraded /dev/mapper/braid-disk1 /mnt/storage")
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "missing" in fi_show.lower(), f"expected missing disk3:\n{fi_show}"

    devid = missing_devid()
    machine.succeed(f"braid remove-missing --missing-id {devid} --yes")
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "missing" not in fi_show.lower(), f"missing device survived removal:\n{fi_show}"
    assert "/dev/mapper/braid-disk3" not in fi_show, f"disk3 still live:\n{fi_show}"

    pool_json = json.loads(machine.succeed("cat /var/lib/braid/pool.json"))
    assert "disk3" not in pool_json["disks"], f"disk3 still in pool.json: {pool_json}"


with subtest("Returned disk3 re-adds without manual wiping"):
    machine.succeed(add_cmd("disk3"))
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    for name in ["braid-disk1", "braid-disk2", "braid-disk3"]:
        assert f"/dev/mapper/{name}" in fi_show, f"{name} missing after re-add:\n{fi_show}"
    assert "missing" not in fi_show.lower(), f"pool still degraded after re-add:\n{fi_show}"

    pool_json = json.loads(machine.succeed("cat /var/lib/braid/pool.json"))
    assert set(pool_json["disks"]) == {"disk1", "disk2", "disk3"}, pool_json
    machine.fail("test -e /var/lib/braid/pending-op.json")


with subtest("Data survives returned-disk re-add"):
    content = machine.succeed("cat /mnt/storage/kept.txt").strip()
    assert content == "returned disk data", f"unexpected data: {content!r}"


machine.shutdown()
