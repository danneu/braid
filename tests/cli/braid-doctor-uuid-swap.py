# Intent: braid doctor's declared_disks check fails when a member disk's
#   live LUKS UUID no longer matches its pool.json key UUID.
# Why it exists: ADR 024 commits to earlier swap detection as a primary
#   doctor surface, but the declared disk classifier previously stopped at
#   LUKS header presence and never verified UUID identity.
# Scenario: a 2-disk RAID1 pool is unlocked and mounted. The operator powers
#   down, swaps disk1 for a different LUKS2 volume in the same physical slot
#   or reformats disk1 by mistake. On reboot, before any unlock attempt,
#   braid doctor must surface the mismatch with Fail severity.

import json
import shlex

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"


def add_cmd(key):
    passphrase_q = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {passphrase_q} | "
        "braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 "
        "--luks-format-arg=--pbkdf-force-iterations "
        f"--luks-format-arg=1000 {key}=/dev/disk/by-id/virtio-{key} "
        "--passphrase-stdin --yes"
    )


with subtest("Build a 2-disk RAID1 pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed("echo 'doctor uuid swap regression data' > /mnt/storage/kept.txt")
    machine.succeed("sync")

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    for name in ["braid-disk1", "braid-disk2"]:
        assert f"/dev/mapper/{name}" in fi_show, f"{name} missing:\n{fi_show}"


with subtest("Reformat disk1 in place while the pool is locked"):
    initial_uuid = machine.succeed(
        "cryptsetup luksUUID /dev/disk/by-id/virtio-disk1"
    ).strip()
    assert initial_uuid != "", "expected disk1 to have a LUKS UUID"

    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup close braid-disk1")
    machine.succeed("cryptsetup close braid-disk2")

    passphrase_q = shlex.quote(passphrase)
    machine.succeed(
        f"printf '%s' {passphrase_q} | cryptsetup luksFormat "
        "--batch-mode --label=braid-disk1 --key-file=- "
        "--pbkdf pbkdf2 --pbkdf-force-iterations 1000 "
        "/dev/disk/by-id/virtio-disk1"
    )

    swapped_uuid = machine.succeed(
        "cryptsetup luksUUID /dev/disk/by-id/virtio-disk1"
    ).strip()
    assert swapped_uuid != initial_uuid, (
        f"reformat should produce a different UUID; old={initial_uuid} "
        f"new={swapped_uuid}"
    )


with subtest("braid doctor fails closed on the UUID mismatch"):
    exit_code, raw = machine.execute("braid doctor --json 2>/tmp/doctor.err")
    assert exit_code != 0, f"doctor must exit non-zero on Fail:\n{raw}"
    report = json.loads(raw)
    assert report["status"] == "fail", f"expected overall fail:\n{raw}"

    checks = {check["name"]: check for check in report["checks"]}
    declared = checks["declared_disks"]
    assert declared["status"] == "fail", f"declared_disks should fail: {declared}"

    message = declared["message"]
    for needle in [
        "disk1",
        f"expected {initial_uuid}",
        f"observed {swapped_uuid}",
        "detach the foreign disk",
    ]:
        assert needle in message, f"missing {needle!r} in:\n{message}"
    assert "disk2" not in message, f"healthy disk2 should not be listed:\n{message}"


machine.shutdown()
