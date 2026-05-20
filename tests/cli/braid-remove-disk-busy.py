# Test: braid-remove-disk-busy
#
# Intent:
#   Verify that `braid remove` exits 0 and surfaces a warning when the LUKS
#   mapper cannot be closed because the device is held busy, and that the
#   mapper remains open afterward.
#
# Why it exists:
#   mapper_close::close_mapper_best_effort, invoked from RemovePlan::execute
#   in cli/src/remove.rs, intentionally treats `cryptsetup close` as
#   best-effort: it warns to stderr and exits 0. The happy-path (clean close)
#   is covered by braid-remove-disk. This test ensures the warning IS emitted
#   and that the contract (exit 0, mapper still open, btrfs device removed)
#   holds when the close fails.
#
# Scenario:
#   A 3-disk RAID1 pool is built. A loop device is set up over
#   /dev/mapper/braid-disk3, which holds an open reference to the mapper and
#   causes `cryptsetup close` to fail with EBUSY. `braid remove disk3` must
#   still succeed: disk3 is removed from btrfs, a warning is printed, and the
#   mapper stays open. Detaching the loop device then allows a clean manual
#   close.

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


with subtest("Setup: build 3-disk RAID1 pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed(add_cmd("disk3"))
    machine.succeed("echo 'important data' > /mnt/storage/precious.txt")

with subtest("Hold mapper busy with a loop device"):
    # losetup --find --show atomically attaches and prints the loop device path.
    # The loop device holds an open fd on braid-disk3, causing cryptsetup close to fail.
    loop_dev = machine.succeed(
        "losetup --find --show /dev/mapper/braid-disk3"
    ).strip()
    assert loop_dev, "expected a loop device to be attached"

with subtest("braid remove exits 0 even when mapper close fails"):
    output = machine.succeed("braid remove disk3 --yes 2>&1")
    wait_row = "[wait] disk disk3: locking..."
    warn_row = "[warn] disk disk3: lock failed"
    assert wait_row in output and warn_row in output, (
        f"expected mapper close wait/warn rows in output:\n{output}"
    )
    assert output.find(wait_row) < output.find(warn_row), (
        f"expected mapper close wait row before warn row:\n{output}"
    )

with subtest("Mapper remains open after busy-close warning"):
    machine.succeed("test -e /dev/mapper/braid-disk3")

with subtest("Pool is otherwise correct — disk3 gone from btrfs"):
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "braid-disk3" not in fi_show, f"disk3 still in btrfs pool:\n{fi_show}"

with subtest("Detaching loop device allows clean luksClose"):
    machine.succeed(f"losetup -d {loop_dev}")
    machine.succeed("cryptsetup close braid-disk3")
    machine.fail("test -e /dev/mapper/braid-disk3")

with subtest("Data intact throughout"):
    content = machine.succeed("cat /mnt/storage/precious.txt").strip()
    assert content == "important data", f"data corrupted: {content}"

machine.shutdown()
