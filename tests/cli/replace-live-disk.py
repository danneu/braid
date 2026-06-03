# Test: replace a live disk in a healthy pool
#
# Intent:
# - What behavior this test (tries to) verify.
#   - `braid replace --old <live> --new <new>` replaces a live, present disk
#     in a healthy pool in place with a single `btrfs replace start` -- the
#     `pool: replacing devid` / `replace complete` progress rows identify the
#     replace-start path, not add+balance+remove -- and closes the old disk's
#     LUKS mapper once the replace completes, leaving the pool healthy and
#     redundant with data and `pool.json` membership intact. The same command
#     also enrolls a keyfile in-step (`--enroll`), and the live path rejects
#     `--missing-id` and refuses to run once the pool has degraded, pointing
#     the operator at the correct full-syntax repair.
#
# Why it exists:
# - What risk/regression this protects against.
#   - Before this feature, `braid replace` only accepted dead/missing disks;
#     live replace is the in-place upgrade path. This test locks that path's
#     operator-visible behavior -- the progress rows, the in-step `--enroll`,
#     and the error/repair guidance -- against silent regression. It is
#     distinct from `replace-preserves-devid.py`, the narrow TDD signal that
#     `btrfs replace start` (not add+balance+remove) was used, proven via the
#     preserved devid. Neither test subsumes the other: deleting either drops
#     real coverage.
#
# Scenario:
# - Real-world situation this models (user/system story). Especially the
#   specific scenario that inspired this test (like a real world bug).
#   - Operator swaps a slow-but-alive drive for a faster one without
#     downtime. The pool stays healthy throughout the operation.

import json


def member_names(pool):
    return {member["name"] for member in pool["disks"].values()}


def member(pool, name):
    for entry in pool["disks"].values():
        if entry["name"] == name:
            return entry
    raise AssertionError(f"{name} missing from pool.json: {pool}")

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


# --- Phase 0: Build 3-drive RAID1 pool ---

with subtest("Setup: build 3-drive pool"):
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

# --- Phase 1: Live replace disk2 → disk4 ---

with subtest("Live replace disk2 with disk4"):
    result = machine.succeed(
        f"{replace_cmd('disk2', 'disk4')} >/tmp/repl.out 2>/tmp/repl.err"
    )
    print(f"braid replace output:\n{result}")
    repl_err = machine.succeed("cat /tmp/repl.err")
    # Principle 13: [wait] rows precede LUKS format, LUKS open, and the
    # btrfs replace start, each closed by a paired [ok] row.
    fmt_wait = "[wait] disk disk4: formatting LUKS..."
    fmt_ok = "[ok]   disk disk4: LUKS formatted"
    assert fmt_wait in repl_err and fmt_ok in repl_err, (
        f"expected LUKS format wait/ok pair, got: {repl_err!r}"
    )
    assert repl_err.find(fmt_wait) < repl_err.find(fmt_ok), (
        f"format wait must precede format ok, got: {repl_err!r}"
    )
    open_wait = "[wait] disk disk4: unlocking..."
    open_ok = "[ok]   disk disk4: unlocked"
    assert open_wait in repl_err and open_ok in repl_err, (
        f"expected LUKS open wait/ok pair, got: {repl_err!r}"
    )
    assert repl_err.find(open_wait) < repl_err.find(open_ok), (
        f"open wait must precede open ok, got: {repl_err!r}"
    )
    repl_wait = "[wait] pool: replacing devid"
    repl_ok = "[ok]   pool: replace complete"
    assert repl_wait in repl_err and repl_ok in repl_err, (
        f"expected replace wait/ok pair, got: {repl_err!r}"
    )
    assert repl_err.find(repl_wait) < repl_err.find(repl_ok), (
        f"replace wait must precede replace ok, got: {repl_err!r}"
    )
    # Principle 13: live-replace closes the old LUKS mapper after replace
    # completes. Pin the [wait]/[ok] pair on the disk-name body (mapper
    # prefix stripped) for cross-command consistency with lock.rs/pool.rs.
    old_close_wait = "[wait] disk disk2: locking..."
    old_close_ok = "[ok]   disk disk2: locked"
    assert old_close_wait in repl_err and old_close_ok in repl_err, (
        f"expected old-mapper close wait/ok pair, got: {repl_err!r}"
    )
    assert repl_err.find(old_close_wait) < repl_err.find(old_close_ok), (
        f"old close wait must precede old close ok, got: {repl_err!r}"
    )
    assert repl_err.find(repl_ok) < repl_err.find(old_close_wait), (
        f"old close must follow replace ok, got: {repl_err!r}"
    )

with subtest("Pool healthy after live replace: disk2 removed, disk4 present"):
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print(f"Pool after live replace:\n{fi_show}")

    assert "/dev/mapper/braid-disk4" in fi_show, (
        f"New disk braid-disk4 missing from pool:\n{fi_show}"
    )
    assert "braid-disk2" not in fi_show, (
        f"Old disk braid-disk2 should be removed:\n{fi_show}"
    )
    assert "missing" not in fi_show.lower(), (
        f"Pool should have no missing devices:\n{fi_show}"
    )
    for name in ["braid-disk1", "braid-disk3"]:
        assert f"/dev/mapper/{name}" in fi_show, (
            f"{name} missing from pool:\n{fi_show}"
        )

    devid_count = fi_show.count("devid")
    assert devid_count == 3, f"Expected 3 devices, got {devid_count}:\n{fi_show}"

    df_output = machine.succeed("btrfs fi df /mnt/storage")
    assert "RAID1" in df_output, f"Expected RAID1 profile:\n{df_output}"

with subtest("Old disk LUKS mapper closed after live replace"):
    machine.fail("test -e /dev/mapper/braid-disk2")

with subtest("Data intact after live replace"):
    content = machine.succeed("cat /mnt/storage/precious.txt").strip()
    assert content == "important data", f"Expected 'important data', got '{content}'"

with subtest("Pool membership updated after live replace"):
    pm = read_pool()
    assert "disk2" not in member_names(pm), f"disk2 still in pool: {pm}"
    assert "disk4" in member_names(pm), f"disk4 missing from pool: {pm}"
    for name in ["disk1", "disk3"]:
        assert name in member_names(pm), f"{name} missing from pool: {pm}"

# --- Phase 1b: Live replace with --enroll (Principle 13 keyfile-enroll row pin) ---
#
# Intent: `braid replace --enroll <kf>` formats the new disk and enrolls the
# keyfile to slot 1, both surfaces wrapped in canonical [wait]/[ok] rows.
# Why it exists: replace.rs's in-flight enroll path is not exercised by any
# other test in the suite -- the prior matrix entry pointed at
# replace-preview-warnings.py which only runs `braid enroll`, not
# `braid replace --enroll`.
# Scenario: operator replaces disk4 with disk5 and enrolls a keyfile for
# unattended unlock in the same step.

with subtest("Live replace disk4 -> disk5 with --enroll"):
    # `braid replace --enroll <dir>` looks for <dir>/braid.key. Use /tmp.
    machine.succeed(
        "dd if=/dev/urandom of=/tmp/braid.key bs=4096 count=1 iflag=fullblock"
    )
    machine.succeed("chmod 400 /tmp/braid.key")
    status, _ = machine.execute(
        f"{replace_cmd('disk4', 'disk5', extra='--enroll /tmp')} >/tmp/repl-en.out 2>/tmp/repl-en.err"
    )
    enroll_out = machine.succeed("cat /tmp/repl-en.out")
    enroll_err = machine.succeed("cat /tmp/repl-en.err")
    assert status == 0, (
        f"replace --enroll failed (exit {status})\n"
        f"stdout: {enroll_out!r}\nstderr: {enroll_err!r}"
    )
    enroll_wait = "[wait] disk disk5: enrolling keyfile in slot 1..."
    enroll_ok = "[ok]   disk disk5: keyfile enrolled in slot 1"
    assert enroll_wait in enroll_err and enroll_ok in enroll_err, (
        f"expected replace --enroll wait/ok pair, got: {enroll_err!r}"
    )
    assert enroll_err.find(enroll_wait) < enroll_err.find(enroll_ok), (
        f"enroll wait must precede enroll ok, got: {enroll_err!r}"
    )

# --- Phase 2: Validation errors ---

with subtest("--missing-id rejected for live disk"):
    (status, output) = machine.execute(
        replace_cmd("disk1", "disk3", extra="--missing-id 99") + " 2>&1"
    )
    assert status != 0, f"Expected failure, got exit 0: {output}"
    assert "--missing-id" in output, f"Expected --missing-id error:\n{output}"

with subtest("Mixed state: simulate dead disk, then live replace fails"):
    # Close disk3 mapper to simulate missing device
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup close braid-disk3")
    machine.succeed("mount -o degraded /dev/mapper/braid-disk1 /mnt/storage")

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "missing" in fi_show.lower(), f"Expected missing device:\n{fi_show}"

    (status, output) = machine.execute(replace_cmd("disk1", "disk3") + " 2>&1")
    assert status != 0, f"Expected failure for mixed state, got exit 0: {output}"
    assert "missing" in output.lower(), f"Expected mention of missing devices:\n{output}"
    assert (
        "braid replace --old <missing-name> --new <new-name>=/dev/disk/by-id/<...>"
        in output
    ), f"Expected full replace repair guidance:\n{output}"
    assert "replace --missing-id" not in output, (
        f"Repair guidance must not request replace --missing-id:\n{output}"
    )

machine.shutdown()
