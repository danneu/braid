# Test: replace a dead (missing) disk
#
# Intent:
# - What behavior this test (tries to) verify.
#   - `braid replace --old <dead> --new <new>` succeeds when the old disk has
#     been physically removed (LUKS mapper closed, device missing from pool).
#     Both auto-detect (devid resolved from `--old`'s pool.json entry) and
#     explicit `--missing-id <devid>` paths use `btrfs replace start` to rebuild
#     from RAID redundancy.
#
# Why it exists:
# - What risk/regression this protects against.
#   - Dead disk replacement is the original braid replace use case. Only unit
#     tests cover the resolution logic; this is the first end-to-end VM test
#     for the dead-disk path.
#
# Scenario:
# - Real-world situation this models.
#   - A drive fails in a 3-drive NAS. The operator plugs in a new drive and
#     runs `braid replace` to swap it in. Later a second drive dies and is
#     replaced with an explicit `--missing-id` to exercise the devid cross-check
#     path.

import json


import re

start_all()
machine.wait_for_unit("multi-user.target")

import shlex

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def read_pool():
    raw = machine.succeed("cat /var/lib/braid/pool.json")
    return json.loads(raw)


def add_cmd(name):
    passphrase_q = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {passphrase_q} | "
        f"braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 {name}=/dev/disk/by-id/virtio-{name} --passphrase-stdin --yes"
    )


def replace_cmd(old, new, extra=""):
    passphrase_q = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {passphrase_q} | "
        f"braid replace --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 --old {old} --new {new}=/dev/disk/by-id/virtio-{new} --passphrase-stdin --yes {extra}"
    )


def get_devid(mapper_name):
    """Extract the btrfs devid for a given mapper from `btrfs fi show`."""
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    for line in fi_show.splitlines():
        if mapper_name in line:
            m = re.search(r"devid\s+(\d+)", line)
            if m:
                return int(m.group(1))
    raise AssertionError(f"devid not found for {mapper_name} in:\n{fi_show}")


# --- Phase 0: Build 3-drive RAID1 pool ---

with subtest("Setup: build 3-drive pool with test data"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed(add_cmd("disk3"))

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    for name in ["braid-disk1", "braid-disk2", "braid-disk3"]:
        assert f"/dev/mapper/{name}" in fi_show, f"{name} missing:\n{fi_show}"

    df_output = machine.succeed("btrfs fi df /mnt/storage")
    assert "RAID1" in df_output, f"Expected RAID1 profile:\n{df_output}"

    machine.succeed("echo 'important data' > /mnt/storage/precious.txt")
    machine.succeed("sync")

# --- Phase 1: Kill disk2, replace with disk4 (auto-detect single missing) ---

with subtest("Kill disk2: simulate drive failure"):
    # Record disk3's devid while pool is healthy (needed for Phase 2)
    disk3_devid = get_devid("braid-disk3")
    print(f"disk3 devid = {disk3_devid}")

    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup close braid-disk2")
    machine.succeed("mount -o degraded /dev/mapper/braid-disk1 /mnt/storage")

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print(f"Pool after disk2 death:\n{fi_show}")
    assert "missing" in fi_show.lower(), f"Expected missing device:\n{fi_show}"

with subtest("Replace dead disk2 with disk4 (auto-detect)"):
    result = machine.succeed(
        f"{replace_cmd('disk2', 'disk4')} >/tmp/repl-dead.out 2>/tmp/repl-dead.err"
    )
    print(f"braid replace output:\n{result}")
    repl_err = machine.succeed("cat /tmp/repl-dead.err")
    # Principle 13: missing-replace path emits the rebuild [wait] and the
    # post-replace RAID1 redundancy restore [wait]/[ok] pair from
    # pool::maybe_restore_raid1.
    rebuild_wait = "[wait] pool: rebuilding missing devid"
    repl_ok = "[ok]   pool: replace complete"
    assert rebuild_wait in repl_err, (
        f"expected rebuild missing wait row, got: {repl_err!r}"
    )
    assert repl_err.find(rebuild_wait) < repl_err.find(repl_ok), (
        f"rebuild wait must precede replace ok, got: {repl_err!r}"
    )
    restore_wait = "[wait] pool: restoring RAID1 redundancy..."
    restore_ok = "[ok]   pool: RAID1 redundancy restored"
    assert restore_wait in repl_err and restore_ok in repl_err, (
        f"expected RAID1 restore wait/ok pair after missing-replace, got: {repl_err!r}"
    )
    assert repl_err.find(restore_wait) < repl_err.find(restore_ok), (
        f"restore wait must precede restore ok, got: {repl_err!r}"
    )

with subtest("Pool healthy after dead replace: disk2 gone, disk4 present"):
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print(f"Pool after replace disk2→disk4:\n{fi_show}")

    assert "/dev/mapper/braid-disk4" in fi_show, (
        f"New disk braid-disk4 missing from pool:\n{fi_show}"
    )
    assert "braid-disk2" not in fi_show, (
        f"Old disk braid-disk2 should be removed:\n{fi_show}"
    )
    assert "missing" not in fi_show.lower(), (
        f"Pool should have no missing devices:\n{fi_show}"
    )

    devid_count = fi_show.count("devid")
    assert devid_count == 3, f"Expected 3 devices, got {devid_count}:\n{fi_show}"

    df_output = machine.succeed("btrfs fi df /mnt/storage")
    assert "RAID1" in df_output, f"Expected RAID1 profile:\n{df_output}"

with subtest("Data intact after dead replace (auto-detect)"):
    content = machine.succeed("cat /mnt/storage/precious.txt").strip()
    assert content == "important data", f"Expected 'important data', got '{content}'"

with subtest("Pool membership updated after dead replace (auto-detect)"):
    pm = read_pool()
    assert "disk2" not in member_names(pm), f"disk2 still in pool: {pm}"
    assert "disk4" in member_names(pm), f"disk4 missing from pool: {pm}"
    for name in ["disk1", "disk3"]:
        assert name in member_names(pm), f"{name} missing from pool: {pm}"

# --- Phase 2: Kill disk3, replace with disk5 (explicit --missing-id) ---

with subtest("Kill disk3: simulate second drive failure"):
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup close braid-disk3")
    machine.succeed("mount -o degraded /dev/mapper/braid-disk1 /mnt/storage")

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print(f"Pool after disk3 death:\n{fi_show}")
    assert "missing" in fi_show.lower(), f"Expected missing device:\n{fi_show}"

with subtest("Wrong --missing-id is rejected early (no pool mutation)"):
    # Wrong --missing-id is caught at validation time, before any LUKS
    # formatting or pool changes. Snapshot pool.json and the btrfs array
    # first so we can prove nothing mutated.
    machine.succeed("cp /var/lib/braid/pool.json /tmp/pool-before-wrong-id.json")
    machine.succeed("btrfs fi show /mnt/storage > /tmp/fi-show-before-wrong-id.txt")

    wrong_devid = 9999
    (status, output) = machine.execute(
        replace_cmd("disk3", "disk5", extra=f"--missing-id {wrong_devid}") + " 2>&1"
    )
    assert status != 0, (
        f"Expected failure with wrong --missing-id {wrong_devid}, got exit 0: {output}"
    )
    print(f"Wrong --missing-id error (expected):\n{output}")

    # Must be the early devid cross-check (OldDevidMismatch), not a late
    # failure after journal/LUKS/btrfs mutation.
    assert "--old and --missing-id disagree" in output, (
        f"Expected the devid-disagreement typo guard, got: {output}"
    )

    # No journal stranded, pool membership untouched, btrfs array untouched,
    # and the new disk was never LUKS-formatted -- proves the rejection
    # landed before execute() ran any mutation.
    machine.fail("test -e /var/lib/braid/pending-op.json")
    machine.succeed("cmp /tmp/pool-before-wrong-id.json /var/lib/braid/pool.json")
    machine.succeed("btrfs fi show /mnt/storage > /tmp/fi-show-after-wrong-id.txt")
    machine.succeed("cmp /tmp/fi-show-before-wrong-id.txt /tmp/fi-show-after-wrong-id.txt")
    machine.fail("cryptsetup isLuks /dev/disk/by-id/virtio-disk5")

with subtest("Replace dead disk3 with disk5 (correct --missing-id)"):
    result = machine.succeed(
        replace_cmd("disk3", "disk5", extra=f"--missing-id {disk3_devid}")
    )
    print(f"braid replace output:\n{result}")

with subtest("Pool healthy after dead replace: disk3 gone, disk5 present"):
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print(f"Pool after replace disk3→disk5:\n{fi_show}")

    assert "/dev/mapper/braid-disk5" in fi_show, (
        f"New disk braid-disk5 missing from pool:\n{fi_show}"
    )
    assert "braid-disk3" not in fi_show, (
        f"Old disk braid-disk3 should be removed:\n{fi_show}"
    )
    assert "missing" not in fi_show.lower(), (
        f"Pool should have no missing devices:\n{fi_show}"
    )

    devid_count = fi_show.count("devid")
    assert devid_count == 3, f"Expected 3 devices, got {devid_count}:\n{fi_show}"

    df_output = machine.succeed("btrfs fi df /mnt/storage")
    assert "RAID1" in df_output, f"Expected RAID1 profile:\n{df_output}"

with subtest("Data intact after dead replace (--missing-id)"):
    content = machine.succeed("cat /mnt/storage/precious.txt").strip()
    assert content == "important data", f"Expected 'important data', got '{content}'"

with subtest("Pool membership updated after dead replace (--missing-id)"):
    pm = read_pool()
    assert "disk3" not in member_names(pm), f"disk3 still in pool: {pm}"
    assert "disk5" in member_names(pm), f"disk5 missing from pool: {pm}"
    for name in ["disk1", "disk4"]:
        assert name in member_names(pm), f"{name} missing from pool: {pm}"

machine.shutdown()
