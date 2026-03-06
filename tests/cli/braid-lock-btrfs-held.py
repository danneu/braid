# Test: braid lock — handles btrfs scan registry on multi-device pool
#
# Intent: `braid lock` successfully closes all LUKS devices on a multi-device
# pool, even with the btrfs kernel scan registry holding references. Cycling
# lock/unlock 3 times exercises the race window to verify reliability.
#
# Why it exists: After umount of a multi-device btrfs, the kernel's scan
# registry retains device references. This can cause `cryptsetup close` to
# fail with "device is busy" in a race window. `braid lock` must call
# `btrfs device scan --forget` after umount to clear the registry reliably.
#
# Scenario: 3-disk pool via `braid add`, write 50 MB of data. Lock/unlock
# 3 times in a loop — each cycle must close all mappers cleanly. Final
# unlock verifies data integrity.

import shlex

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def add_cmd(key):
    pq = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {pq} | "
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid add {key} --passphrase-stdin --yes"
    )


def unlock_cmd():
    pq = shlex.quote(passphrase)
    return f"printf '%s\\n' {pq} | braid unlock --passphrase-stdin"


# --- Setup ---

with subtest("Setup: create 3-disk pool with test data"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed(add_cmd("disk3"))
    machine.succeed("dd if=/dev/urandom of=/mnt/storage/testfile.bin bs=1M count=50")
    machine.succeed("sync")
    machine.succeed("md5sum /mnt/storage/testfile.bin > /tmp/checksum.txt")

# --- Test: 3 lock/unlock cycles ---

for i in range(1, 4):
    with subtest(f"Cycle {i}: braid lock closes all mappers"):
        machine.succeed("braid lock")

        # All mappers must be closed
        machine.fail("mountpoint -q /mnt/storage")
        for k in ["disk1", "disk2", "disk3"]:
            machine.fail(f"test -e /dev/mapper/braid-{k}")

    with subtest(f"Cycle {i}: braid unlock restores pool"):
        machine.succeed(unlock_cmd())
        machine.succeed("mountpoint -q /mnt/storage")

# --- Final: verify data integrity ---

with subtest("Data integrity after 3 lock/unlock cycles"):
    machine.succeed("md5sum -c /tmp/checksum.txt")

machine.shutdown()
