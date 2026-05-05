# Test: braid replace emits no "Old device closed" when post-replace close fails
#
# Intent:
#   Verify that `braid replace` on a live disk exits 0 and prints only the
#   close-failure warning (NOT the "Old device closed. ... wipe it
#   separately." follow-up) when the best-effort `cryptsetup close` of the
#   old mapper fails with EBUSY. Also verify the btrfs-level replace itself
#   still completed and the old mapper remains open.
#
# Why it exists:
#   Regression guard for a bug where the wipe-guidance line printed
#   unconditionally after the match on close_result in replace.rs. That
#   produced contradictory output on a real close failure -- a warning that
#   the mapper did NOT close, followed by guidance to wipe the disk -- and
#   was actively dangerous: an operator who acted on the wipe guidance
#   while the mapper was still open on live data would destroy the source
#   disk's contents.
#
# Scenario:
#   A 3-disk RAID1 pool is built (disk1, disk2, disk3). A loop device is
#   attached to /dev/mapper/braid-disk2 to hold it busy; the btrfs replace
#   itself proceeds fine (it routes I/O through the mount point, not via
#   exclusive access to the source mapper), but the subsequent
#   `cryptsetup close braid-disk2` fails with EBUSY. `braid replace` must
#   still exit 0, print a warning naming braid-disk2, and suppress the
#   contradictory "Old device closed" line. The mapper stays open; btrfs
#   sees disk4 in place of disk2; data is intact.

import json
import shlex

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def add_cmd(name):
    passphrase_q = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {passphrase_q} | "
        f"braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 {name}=/dev/disk/by-id/virtio-{name} --passphrase-stdin --yes"
    )


def replace_cmd(old, new):
    passphrase_q = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {passphrase_q} | "
        f"braid replace --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 --old {old} --new {new}=/dev/disk/by-id/virtio-{new} "
        f"--passphrase-stdin --yes"
    )


with subtest("Setup: build 3-disk RAID1 pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed(add_cmd("disk3"))
    machine.succeed("echo 'important data' > /mnt/storage/precious.txt")
    machine.succeed("sync")

with subtest("Hold old mapper busy with a loop device"):
    # losetup --find --show atomically attaches and prints the loop device path.
    # The loop device holds an open fd on braid-disk2, causing the
    # post-replace cryptsetup close to fail with EBUSY.
    loop_dev = machine.succeed(
        "losetup --find --show /dev/mapper/braid-disk2"
    ).strip()
    assert loop_dev, "expected a loop device to be attached"

with subtest("braid replace exits 0 even when post-replace luksClose fails"):
    output = machine.succeed(replace_cmd("disk2", "disk4") + " 2>&1")
    print(f"braid replace output:\n{output}")

    assert "Warning" in output and "braid-disk2" in output, (
        f"expected luksClose warning naming braid-disk2:\n{output}"
    )
    # Regression gate: the wipe-guidance line must NOT appear when the
    # close failed. Before the fix, it printed unconditionally.
    assert "Old device closed" not in output, (
        f"'Old device closed' must not print when close fails:\n{output}"
    )

with subtest("Old mapper remains open after busy-close warning"):
    machine.succeed("test -e /dev/mapper/braid-disk2")

with subtest("btrfs replace itself succeeded: disk2 gone, disk4 present"):
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "/dev/mapper/braid-disk4" in fi_show, (
        f"new disk braid-disk4 missing from pool:\n{fi_show}"
    )
    assert "braid-disk2" not in fi_show, (
        f"old disk braid-disk2 should be removed from pool:\n{fi_show}"
    )
    assert "missing" not in fi_show.lower(), (
        f"pool should have no missing devices:\n{fi_show}"
    )

with subtest("Pool membership updated"):
    pm = json.loads(machine.succeed("cat /var/lib/braid/pool.json"))
    assert "disk2" not in pm["disks"], f"disk2 still in pool: {pm}"
    assert "disk4" in pm["disks"], f"disk4 missing from pool: {pm}"

with subtest("Data intact throughout"):
    content = machine.succeed("cat /mnt/storage/precious.txt").strip()
    assert content == "important data", f"data corrupted: {content}"

with subtest("Detaching loop device allows clean luksClose"):
    machine.succeed(f"losetup -d {loop_dev}")
    machine.succeed("cryptsetup close braid-disk2")
    machine.fail("test -e /dev/mapper/braid-disk2")

machine.shutdown()
