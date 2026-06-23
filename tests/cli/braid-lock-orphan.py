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

import json
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
        f"braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 {key}=/dev/disk/by-id/virtio-{key} --passphrase-stdin --yes"
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
    pool_raw = machine.succeed("cat /var/lib/braid/pool.json")
    pool = json.loads(pool_raw)
    members = member_names(pool)
    assert members == {"disk1", "disk2"}, f"expected {{disk1, disk2}}, got {members}"

with subtest("Pre-lock: all three mappers exist"):
    machine.succeed("test -e /dev/mapper/braid-disk1")
    machine.succeed("test -e /dev/mapper/braid-disk2")
    machine.succeed("test -e /dev/mapper/braid-orphan")

with subtest("braid lock closes membership mappers and orphan"):
    machine.succeed("braid lock >/tmp/lock-orphan.out 2>/tmp/lock-orphan.err")
    lock_err = machine.succeed("cat /tmp/lock-orphan.err")
    # Principle 13: the orphan-mapper close path emits its own [wait] row
    # before cryptsetup close, closed by the existing [ok] orphan row.
    # Both rows use the user-friendly disk_name (mapper name stripped of
    # the braid- prefix) so subject pairing is consistent.
    orphan_warn = (
        "[warn] orphaned mapper braid-orphan "
        "(not in pool.json -- likely a prior crash)"
    )
    orphan_wait = "[wait] disk orphan: locking (orphan)..."
    orphan_ok = "[ok]   disk orphan: locked (orphan)"
    pool_wait = "[wait] pool: unmounting /mnt/storage..."
    assert lock_err.count(orphan_warn) == 1, (
        f"expected exactly one orphan warning, got: {lock_err!r}"
    )
    assert lock_err.find(orphan_warn) < lock_err.find(pool_wait), (
        f"orphan warning must precede first work row, got: {lock_err!r}"
    )
    assert orphan_wait in lock_err, (
        f"expected orphan locking wait row, got: {lock_err!r}"
    )
    assert lock_err.find(orphan_wait) < lock_err.find(orphan_ok), (
        f"orphan wait must precede orphan ok, got: {lock_err!r}"
    )

    # Pool unmounted
    machine.fail("mountpoint -q /mnt/storage")

    # Membership-known mappers closed
    machine.fail("test -e /dev/mapper/braid-disk1")
    machine.fail("test -e /dev/mapper/braid-disk2")

    # Orphan mapper closed
    machine.fail("test -e /dev/mapper/braid-orphan")

machine.shutdown()
