# Test: braid doctor offline declared member
#
# Intent: verify declared_disks distinguishes identity-verified members that
#   are absent from a mounted live pool from normal healthy members.
# Why it exists: doctor used to report these verified-but-unassembled members
#   as healthy because it only checked LUKS identity.
# Scenario: a NAS remounts a RAID1 pool degraded after one member mapper is
#   closed, while the raw by-id disk remains present and its LUKS UUID matches
#   pool.json.

import json
import shlex


start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"


def add_cmd(key):
    passphrase_q = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {passphrase_q} | "
        f"braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 "
        f"--luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 "
        f"{key}=/dev/disk/by-id/virtio-{key} --passphrase-stdin --yes"
    )


def declared_disks_check():
    raw = machine.succeed("braid doctor --json")
    print(f"Doctor JSON:\n{raw}")
    report = json.loads(raw)
    checks = {check["name"]: check for check in report["checks"]}
    return checks["declared_disks"]


# Intent: a fully assembled mounted pool reports declared disks as healthy.
# Why it exists: the live-pool cross-check must not create false warnings when
#   every declared member is assembled.
# Scenario: an operator has just added both disks and the RAID1 pool is mounted.
with subtest("Mounted all assembled -- ok"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed("mountpoint -q /mnt/storage")
    df = machine.succeed("btrfs filesystem df /mnt/storage")
    assert "RAID1" in df, f"Expected RAID1 after adding disk2:\n{df}"

    check = declared_disks_check()

    assert check["status"] == "ok", f"declared_disks: {check}"


# Intent: a present, identity-verified member absent from the live pool warns.
# Why it exists: this is the exact status-side offline state doctor previously
#   misreported as healthy.
# Scenario: disk2's mapper is closed before a degraded remount, but its raw
#   by-id device still exists and its LUKS header verifies.
with subtest("Mounted one member dropped -- warn"):
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup close braid-disk2")
    machine.succeed("mount -o degraded /dev/mapper/braid-disk1 /mnt/storage")
    machine.succeed("mountpoint -q /mnt/storage")

    check = declared_disks_check()

    assert check["status"] == "warn", f"declared_disks: {check}"
    assert "disk2" in check["message"], f"Expected disk2 in message: {check}"
    assert "not in the live pool" in check["message"], (
        f"Expected offline wording: {check}"
    )


# Intent: an unmounted pool preserves declared_disks identity-only behavior.
# Why it exists: without live btrfs topology there is nothing to compare
#   against, so verified raw members should still be healthy.
# Scenario: the NAS pool is locked or offline while its declared disks remain
#   attached and their LUKS headers are readable.
with subtest("Pool offline -- ok"):
    machine.succeed("umount /mnt/storage")

    check = declared_disks_check()

    assert check["status"] == "ok", f"declared_disks: {check}"


machine.shutdown()
