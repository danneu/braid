# Intent: braid doctor's foreign_luks_uuid check fails when the live btrfs
#   pool admits a LUKS device whose UUID is not in pool.json.
# Why it exists: enrich_from_pool_state detects foreign live UUIDs but the
#   eprintln warning scrolls off-screen; doctor must surface the structured
#   diagnosis the status hint already promises.
# Scenario: A healthy 2-disk RAID1 pool is mounted. The operator or a stray
#   cryptsetup session freshly luksFormats disk3, opens it as a mapper, and
#   force-adds it to btrfs. braid doctor --json must report the
#   foreign_luks_uuid check as fail and exit non-zero.

import json
import shlex

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
pp = shlex.quote(passphrase)


def add_cmd(name, disk):
    return (
        f"printf '%s\\n' {pp} | "
        "braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 "
        "--luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 "
        f"{name}=/dev/disk/by-id/virtio-{disk} --passphrase-stdin --yes"
    )


with subtest("Build a 2-disk RAID1 pool"):
    machine.succeed(add_cmd("disk1", "disk1"))
    machine.succeed(add_cmd("disk2", "disk2"))


with subtest("doctor is clean before foreign UUID is injected"):
    raw = machine.succeed("braid doctor --json")
    report = json.loads(raw)
    checks = {check["name"]: check for check in report["checks"]}
    assert "foreign_luks_uuid" in checks, f"check missing: {list(checks)}"
    assert checks["foreign_luks_uuid"]["status"] == "ok", checks["foreign_luks_uuid"]


with subtest("luksFormat disk3 with a fresh UUID"):
    machine.succeed(
        f"printf '%s' {pp} | "
        "cryptsetup luksFormat --batch-mode --pbkdf pbkdf2 "
        "--pbkdf-force-iterations 1000 --key-file=- "
        "/dev/disk/by-id/virtio-disk3"
    )
    machine.succeed(
        f"printf '%s' {pp} | "
        "cryptsetup open --key-file=- /dev/disk/by-id/virtio-disk3 braid-stranger"
    )


with subtest("Force-add foreign mapper into the live pool"):
    machine.succeed("btrfs device add -f /dev/mapper/braid-stranger /mnt/storage")
    filesystem_show = machine.succeed("btrfs filesystem show /mnt/storage")
    assert "braid-stranger" in filesystem_show, filesystem_show


with subtest("braid doctor fails with foreign_luks_uuid Fail"):
    exit_code, stdout = machine.execute("braid doctor --json 2>/tmp/braid-doctor.err")
    assert exit_code != 0, f"doctor must exit non-zero on Fail:\n{stdout}"
    report = json.loads(stdout)
    assert report["status"] == "fail", report["status"]

    checks = {check["name"]: check for check in report["checks"]}
    check = checks["foreign_luks_uuid"]
    assert check["status"] == "fail", check
    message = check["message"]
    for needle in [
        "foreign LUKS UUID",
        "braid-stranger",
        "btrfs device remove /dev/mapper/braid-stranger",
    ]:
        assert needle in message, f"missing {needle!r} in:\n{message}"
    assert message.find("btrfs device remove") < message.find("cryptsetup close"), message
    assert "<mapper>" not in message, f"placeholder leaked in:\n{message}"

    stderr = machine.succeed("cat /tmp/braid-doctor.err")
    assert "Warning: live LUKS UUID" not in stderr, (
        "doctor regressed into the enrich_from_pool_state warning path; "
        f"stderr was:\n{stderr}"
    )


machine.shutdown()
