# Test: replace rejects an undersized target before destructive work
#
# Intent:
# - `braid replace` refuses a replacement whose modeled mapper capacity is
#   smaller than the source device's btrfs `total_bytes`, for both a live
#   source and a missing source.
#
# Why it exists:
# - btrfs already rejects this at `btrfs replace start`, but that used to
#   happen after braid wrote `pending-op.json` and LUKS-formatted a fresh
#   target. The regression guard is that braid refuses before both effects.
#
# Scenario:
# - A 2-disk 512 MiB RAID1 pool is offered a 256 MiB replacement. The operator
#   first tries while disk2 is still live, then after disk2 has been removed
#   and btrfs reports it as missing.

import re
import shlex


start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"


def add_cmd(name):
    passphrase_q = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {passphrase_q} | "
        f"braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 "
        f"--luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 "
        f"{name}=/dev/disk/by-id/virtio-{name} --passphrase-stdin --yes"
    )


def replace_cmd(extra=""):
    passphrase_q = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {passphrase_q} | "
        f"braid replace --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 "
        f"--luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 "
        f"--old disk2 --new disk3=/dev/disk/by-id/virtio-disk3 "
        f"--passphrase-stdin --yes {extra}"
    )


def disk2_devid():
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    for line in fi_show.splitlines():
        if "/dev/mapper/braid-disk2" in line:
            match = re.search(r"devid\s+(\d+)", line)
            assert match, f"could not parse devid from line: {line}"
            return int(match.group(1))
    raise AssertionError(f"disk2 not found in btrfs fi show:\n{fi_show}")


def assert_refused_without_side_effects(output):
    assert "smaller than the disk being replaced" in output, (
        f"expected size refusal, got:\n{output}"
    )
    machine.fail("test -e /var/lib/braid/pending-op.json")
    machine.fail("cryptsetup isLuks /dev/disk/by-id/virtio-disk3")


with subtest("Setup: build 2-drive pool with 512 MiB members"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed("echo 'important data' > /mnt/storage/precious.txt")
    machine.succeed("sync")

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "/dev/mapper/braid-disk1" in fi_show, fi_show
    assert "/dev/mapper/braid-disk2" in fi_show, fi_show
    old_devid = disk2_devid()


with subtest("Live source: undersized fresh-LUKS target is refused before format"):
    status, output = machine.execute(replace_cmd() + " 2>&1")
    assert status != 0, f"expected replace refusal, got exit 0:\n{output}"
    assert_refused_without_side_effects(output)


with subtest("Make disk2 missing"):
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup close braid-disk2")
    machine.succeed("mount -o degraded /dev/mapper/braid-disk1 /mnt/storage")
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "missing" in fi_show.lower(), fi_show


with subtest("Missing source: undersized fresh-LUKS target is refused before format"):
    status, output = machine.execute(replace_cmd(extra=f"--missing-id {old_devid}") + " 2>&1")
    assert status != 0, f"expected replace refusal, got exit 0:\n{output}"
    assert_refused_without_side_effects(output)


with subtest("Pool data remains readable"):
    content = machine.succeed("cat /mnt/storage/precious.txt").strip()
    assert content == "important data", content


machine.shutdown()
