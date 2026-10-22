# Test: replace rejects an undersized target before destructive work
#
# Intent:
# - `braid replace` refuses a replacement whose modeled mapper capacity is
#   smaller than the source device's btrfs `total_bytes`, for both a live
#   source and a missing source.
# - A fresh target the preflight accepts really fits: braid formats it at the
#   data offset the preflight models, even when the disk's optimal I/O size
#   would make cryptsetup's own layout start the data later.
#
# Why it exists:
# - btrfs already rejects this at `btrfs replace start`, but that used to
#   happen after braid wrote `pending-op.json` and LUKS-formatted a fresh
#   target. The regression guard is that braid refuses before both effects.
# - The preflight once assumed cryptsetup's default 16 MiB offset while
#   luksFormat let cryptsetup round it up to the optimal I/O size. An
#   equal-size disk passed the preflight, got a 32 MiB offset, and btrfs
#   refused it after the journal and the format (finding 261022-0632-012).
#
# Scenario:
# - A 2-disk 512 MiB RAID1 pool is offered a 256 MiB replacement for disk2
#   while disk2 is live.
# - The operator replaces disk2 with a 512 MiB disk4 that reports a 32 MiB
#   optimal I/O size, like a RAID LUN or a USB bridge.
# - disk4 is then removed, btrfs reports it as missing, and the operator
#   offers the 256 MiB disk again.

import json
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


def replace_cmd(old, new, extra=""):
    passphrase_q = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {passphrase_q} | "
        f"braid replace --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 "
        f"--luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 "
        f"--old {old} --new {new}=/dev/disk/by-id/virtio-{new} "
        f"--passphrase-stdin --yes {extra}"
    )


def member_devid(name):
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    for line in fi_show.splitlines():
        if f"/dev/mapper/braid-{name}" in line:
            match = re.search(r"devid\s+(\d+)", line)
            assert match, f"could not parse devid from line: {line}"
            return int(match.group(1))
    raise AssertionError(f"{name} not found in btrfs fi show:\n{fi_show}")


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


with subtest("Live source: undersized fresh-LUKS target is refused before format"):
    status, output = machine.execute(replace_cmd("disk2", "disk3") + " 2>&1")
    assert status != 0, f"expected replace refusal, got exit 0:\n{output}"
    assert_refused_without_side_effects(output)


with subtest("Live source: equal-size target with a 32 MiB optimal I/O size fits"):
    # Precondition: without the topology hint this subtest would pass
    # without exercising cryptsetup's alignment rounding at all.
    opt_io = machine.succeed(
        "cat /sys/class/block/$(basename $(readlink -f /dev/disk/by-id/virtio-disk4))"
        "/queue/optimal_io_size"
    ).strip()
    assert opt_io == "33554432", f"disk4 must report a 32 MiB optimal I/O size, got {opt_io!r}"

    status, output = machine.execute(replace_cmd("disk2", "disk4") + " 2>&1")
    print(f"braid replace exit {status}:\n{output}")

    # The offset is the cause; check it before the outcome so a regression
    # names it rather than only btrfs's later size refusal.
    dump = json.loads(
        machine.succeed("cryptsetup luksDump --dump-json-metadata /dev/disk/by-id/virtio-disk4")
    )
    offset = dump["segments"]["0"]["offset"]
    assert offset == "16777216", f"expected the pinned 16 MiB data offset, got {offset!r}"

    assert status == 0, f"expected equal-size replace to succeed, got exit {status}:\n{output}"
    machine.fail("test -e /var/lib/braid/pending-op.json")

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "/dev/mapper/braid-disk4" in fi_show, fi_show
    assert "/dev/mapper/braid-disk2" not in fi_show, fi_show


with subtest("Make disk4 missing"):
    old_devid = member_devid("disk4")
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup close braid-disk4")
    machine.succeed("mount -o degraded /dev/mapper/braid-disk1 /mnt/storage")
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "missing" in fi_show.lower(), fi_show


with subtest("Missing source: undersized fresh-LUKS target is refused before format"):
    status, output = machine.execute(
        replace_cmd("disk4", "disk3", extra=f"--missing-id {old_devid}") + " 2>&1"
    )
    assert status != 0, f"expected replace refusal, got exit 0:\n{output}"
    assert_refused_without_side_effects(output)


with subtest("Pool data remains readable"):
    content = machine.succeed("cat /mnt/storage/precious.txt").strip()
    assert content == "important data", content


machine.shutdown()
