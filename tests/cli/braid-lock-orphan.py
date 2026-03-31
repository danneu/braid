# Test: braid lock orphan mapper cleanup
#
# Intent: Verify `braid lock` closes orphaned braid-* mappers that exist in
# /dev/mapper but are not listed in pool.json.
#
# Why it exists: If a crash occurs after `cryptsetup open` but before the
# journal or pool.json is written (e.g., power loss during `braid add`),
# a braid-* mapper is left open with no corresponding pool.json entry.
# The membership-only iteration in the lock command would miss it. The
# supplementary /dev/mapper scan added in lock.rs closes this gap.
#
# Scenario: 2-disk RAID1 pool created via `braid add`. A third disk is
# LUKS-formatted and opened as braid-orphan outside of braid (simulating
# the crash window). `braid lock` must close all three mappers — the two
# membership-known ones and the orphan — and leave the system fully locked.

import shlex

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def add_cmd(key):
    """Build a `braid add <key> --yes` command."""
    pq = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {pq} | "
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid add {key}=/dev/disk/by-id/virtio-{key} --passphrase-stdin --yes"
    )


# --- Setup: Create a 2-disk RAID1 pool ---

with subtest("Setup: create 2-disk pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed("mountpoint -q /mnt/storage")

# --- Setup: simulate crash window orphan ---
# LUKS-format disk3 and open it as braid-orphan, but do NOT add it to
# pool.json. This models the state after a crash between cryptsetup open
# and journal/pool.json write during `braid add`.

with subtest("Setup: create orphan mapper (crash window simulation)"):
    pq = shlex.quote(passphrase)
    machine.succeed(
        f"printf '%s\\n' {pq} | "
        f"cryptsetup luksFormat --batch-mode --key-file=- "
        f"{luks_opts} /dev/disk/by-id/virtio-disk3"
    )
    machine.succeed(
        f"printf '%s\\n' {pq} | "
        f"cryptsetup open --key-file=- "
        f"/dev/disk/by-id/virtio-disk3 braid-orphan"
    )
    machine.succeed("test -e /dev/mapper/braid-orphan")

# --- Test: braid lock closes orphan and membership mappers ---

with subtest("Pre-lock: orphan is outside pool.json membership"):
    import json

    pool_raw = machine.succeed("cat /var/lib/braid/pool.json")
    pool = json.loads(pool_raw)
    members = set(pool["disks"].keys())
    assert members == {"disk1", "disk2"}, f"expected {{disk1, disk2}}, got {members}"

with subtest("Pre-lock: all three mappers exist"):
    machine.succeed("test -e /dev/mapper/braid-disk1")
    machine.succeed("test -e /dev/mapper/braid-disk2")
    machine.succeed("test -e /dev/mapper/braid-orphan")

with subtest("braid lock closes membership mappers and orphan"):
    machine.succeed("braid lock")

    # Pool unmounted
    machine.fail("mountpoint -q /mnt/storage")

    # Membership-known mappers closed
    machine.fail("test -e /dev/mapper/braid-disk1")
    machine.fail("test -e /dev/mapper/braid-disk2")

    # Orphan mapper closed
    machine.fail("test -e /dev/mapper/braid-orphan")

machine.shutdown()
