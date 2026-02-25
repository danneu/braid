# Test: braid-remove-disk-busy
#
# Intent:
#   Verify that `braid remove` exits 0 and surfaces a warning when the LUKS
#   mapper cannot be closed because another process holds it open, and that
#   the mapper remains open afterward.
#
# Why it exists:
#   pool.rs intentionally treats `cryptsetup close` as best-effort: it warns
#   to stderr and exits 0. The happy-path (clean close) is covered by
#   braid-remove-disk. This test ensures the warning IS emitted and that the
#   contract (exit 0, mapper still open, btrfs device removed) holds when the
#   close fails.
#
# Scenario:
#   A 3-disk RAID1 pool is built. A background `sleep` holds an open fd on
#   /dev/mapper/braid-disk3, causing `cryptsetup close` to fail. `braid remove
#   disk3` must still succeed: disk3 is removed from btrfs, a warning is
#   printed, and the mapper stays open. Killing the holder then allows a clean
#   manual close.

import shlex

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def add_cmd(key):
    passphrase_q = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {passphrase_q} | "
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid add {key} --passphrase-stdin --yes"
    )


with subtest("Setup: build 3-disk RAID1 pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed(add_cmd("disk3"))
    machine.succeed("echo 'important data' > /mnt/storage/precious.txt")

with subtest("Hold mapper open with background process"):
    # sleep holds an open fd on /dev/mapper/braid-disk3, making cryptsetup close fail
    machine.succeed("sleep 3600 < /dev/mapper/braid-disk3 &")

with subtest("braid remove exits 0 even when luksClose fails"):
    output = machine.succeed("braid remove disk3 --yes 2>&1")
    assert "Warning" in output and "braid-disk3" in output, (
        f"expected luksClose warning in output:\n{output}"
    )

with subtest("Mapper remains open after busy-close warning"):
    machine.succeed("test -e /dev/mapper/braid-disk3")

with subtest("Pool is otherwise correct — disk3 gone from btrfs"):
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "braid-disk3" not in fi_show, f"disk3 still in btrfs pool:\n{fi_show}"

with subtest("Closing the holder allows clean luksClose"):
    machine.succeed("pkill -f 'sleep 3600' || true")
    machine.succeed("cryptsetup close braid-disk3")
    machine.fail("test -e /dev/mapper/braid-disk3")

with subtest("Data intact throughout"):
    content = machine.succeed("cat /mnt/storage/precious.txt").strip()
    assert content == "important data", f"data corrupted: {content}"

machine.shutdown()
