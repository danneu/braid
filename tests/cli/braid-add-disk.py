# Test: braid add lifecycle
#
# What: Runs `braid add <name> --yes` through its full lifecycle: first disk
# (creates pool), second disk (converts to RAID1), third disk (expands pool),
# validation errors, idempotent re-add, identity-check refusals (non-braid
# LUKS, braid-labeled but no btrfs), disk add after cleanup, and fifth disk
# expansion.
#
# Why: `braid add` is the primary intent command for LUKS format + pool join.
# Every primitive has been proven in isolation (luks, btrfs-raid1, grow, shrink,
# heal, degrade). This test proves the intent CLI ties them together correctly
# and that the identity checks refuse unsafe disks.
#
# Dependencies: btrfs-grow1 (single -> RAID1 -> 3-drive works manually).

start_all()
machine.wait_for_unit("multi-user.target")

import shlex

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def add_cmd(key):
    """Build a `braid add <key> --yes` command with env vars."""
    passphrase_q = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {passphrase_q} | "
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid add {key}=/dev/disk/by-id/virtio-{key} --passphrase-stdin --yes"
    )


# --- Phase 1: First disk (no pool) ---

with subtest("First disk creates single-drive pool"):
    machine.succeed(
        f"{add_cmd('disk1')} >/tmp/add1.out 2>/tmp/add1.err"
    )
    add1_err = machine.succeed("cat /tmp/add1.err")

    # Principle 13: [wait] before each cryptsetup Argon2 step.
    fmt_wait = "[wait] disk disk1: formatting LUKS..."
    fmt_ok = "[ok]   disk disk1: LUKS formatted"
    assert fmt_wait in add1_err and fmt_ok in add1_err, (
        f"expected LUKS format wait/ok pair, got: {add1_err!r}"
    )
    assert add1_err.find(fmt_wait) < add1_err.find(fmt_ok), (
        f"format wait must precede format ok, got: {add1_err!r}"
    )
    open_wait = "[wait] disk disk1: unlocking..."
    open_ok = "[ok]   disk disk1: unlocked"
    assert open_wait in add1_err and open_ok in add1_err, (
        f"expected LUKS open wait/ok pair, got: {add1_err!r}"
    )
    assert add1_err.find(open_wait) < add1_err.find(open_ok), (
        f"open wait must precede open ok, got: {add1_err!r}"
    )

    # Pool is mounted
    machine.succeed("mountpoint -q /mnt/storage")

    # Single profile (only 1 drive)
    df_output = machine.succeed("btrfs fi df /mnt/storage")
    assert "Data, single" in df_output, f"Expected single profile:\n{df_output}"
    assert "Metadata, DUP" in df_output, f"Expected DUP metadata profile:\n{df_output}"

    # LUKS mapper exists with correct name (braid-<key>)
    machine.succeed("test -e /dev/mapper/braid-disk1")

    # Can write data
    machine.succeed("echo 'day one data' > /mnt/storage/day1.txt")
    machine.succeed("sync")

# --- Phase 2: Second disk (convert to RAID1) ---

with subtest("Second disk converts pool to RAID1"):
    machine.succeed(
        f"{add_cmd('disk2')} >/tmp/add2.out 2>/tmp/add2.err"
    )
    add2_err = machine.succeed("cat /tmp/add2.err")
    # Adding a second disk to a 1-disk pool triggers pool_balance_raid1.
    bal_wait = "[wait] pool: balancing to RAID1..."
    bal_ok = "[ok]   pool: RAID1 balance complete"
    assert bal_wait in add2_err and bal_ok in add2_err, (
        f"expected RAID1 balance wait/ok pair, got: {add2_err!r}"
    )
    assert add2_err.find(bal_wait) < add2_err.find(bal_ok), (
        f"balance wait must precede balance ok, got: {add2_err!r}"
    )

    df_output = machine.succeed("btrfs fi df /mnt/storage")
    assert "Data, RAID1" in df_output, f"Expected RAID1:\n{df_output}"

with subtest("Day 1 data survived RAID1 conversion"):
    content = machine.succeed("cat /mnt/storage/day1.txt").strip()
    assert content == "day one data", f"Expected 'day one data', got '{content}'"

with subtest("Write more data on RAID1"):
    machine.succeed("echo 'day two data' > /mnt/storage/day2.txt")
    machine.succeed("sync")

# --- Phase 3: Third disk (add to RAID1) ---

with subtest("Third disk expands RAID1 pool"):
    machine.succeed(add_cmd("disk3"))

    # All 3 mapper devices in pool
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    for name in ["braid-disk1", "braid-disk2", "braid-disk3"]:
        assert f"/dev/mapper/{name}" in fi_show, f"{name} missing:\n{fi_show}"

with subtest("All data survived third disk addition"):
    content1 = machine.succeed("cat /mnt/storage/day1.txt").strip()
    content2 = machine.succeed("cat /mnt/storage/day2.txt").strip()
    assert content1 == "day one data", f"Expected 'day one data', got '{content1}'"
    assert content2 == "day two data", f"Expected 'day two data', got '{content2}'"

# --- Phase 4: Validation errors ---

with subtest("Non-existent key fails add"):
    machine.fail(add_cmd("nonexistent"))

with subtest("Already-in-pool disk is a no-op (exit 0)"):
    # Intent: `braid add <already-in-pool> --dry-run` is a note-only
    # success: stdout carries exactly the canonical "Nothing to do --
    # <name> already in pool." line, stderr is empty, and no step lines
    # leak onto stdout.
    # Why it exists: PR 7 routes dry-run through the shared Preview
    # model. The already-in-pool branch is a zero-step + Info-note
    # preview. A regression that stacked `nothing to do.` (generic
    # fallback) on top of the Info note, or routed the note back to
    # stderr, would still exit 0 and slip past existing coverage.
    # Scenario: disk1 was added in Phase 1 and is already a pool member.
    machine.succeed(
        add_cmd("disk1").replace("--yes", "--yes --dry-run")
        + " >/tmp/noop-stdout 2>/tmp/noop-stderr"
    )
    out = machine.succeed("cat /tmp/noop-stdout")
    err = machine.succeed("cat /tmp/noop-stderr")
    assert out == "Nothing to do -- disk1 already in pool.\n", (
        "dry-run no-op stdout must be exactly the noop Info note; got: {!r}".format(out)
    )
    assert err == "", (
        "dry-run no-op stderr must be empty; got: {!r}".format(err)
    )
    assert "nothing to do." not in out, (
        "generic fallback must not stack with the Info note; got: {!r}".format(out)
    )

    # Real-run no-op: wording preservation on stderr. Today's message
    # "Nothing to do -- <name> already in pool." must stay byte-identical
    # so log scrapers don't drift after the Preview-model migration.
    machine.succeed(
        add_cmd("disk1") + " >/tmp/rnoop-stdout 2>/tmp/rnoop-stderr"
    )
    out = machine.succeed("cat /tmp/rnoop-stdout")
    err = machine.succeed("cat /tmp/rnoop-stderr")
    assert "Nothing to do -- disk1 already in pool." in err, (
        "real-run no-op stderr must contain the canonical wording; got: {!r}".format(err)
    )
    assert out == "", (
        "real-run no-op stdout must be empty; got: {!r}".format(out)
    )

# --- Phase 5: Identity check refusals + fresh add after cleanup ---

with subtest("Non-braid LUKS disk is refused"):
    # Intent: braid add refuses a LUKS device without the braid-<name> label.
    # Why: adopting unknown LUKS containers risks merging unrelated filesystems.
    # Scenario: user has a LUKS-encrypted drive from another system and tries
    # to add it to braid.
    dev = "/dev/disk/by-id/virtio-disk4"
    machine.succeed(
        f"echo -n '{passphrase}' | cryptsetup luksFormat --batch-mode --key-file=- "
        f"{luks_opts} {dev}"
    )
    machine.fail(add_cmd("disk4"))

    # Clean up: wipe the LUKS header so disk4 is fresh for the next test
    machine.succeed(f"dd if=/dev/zero of={dev} bs=1M count=4")

with subtest("Braid-labeled LUKS with no btrfs is refused"):
    # Intent: braid add refuses a braid-labeled LUKS device that has no btrfs
    # superblock (partial/inconsistent state).
    # Why: a braid label without btrfs data means the disk was never fully
    # initialized. This could be a crash between luksFormat and mkfs, or
    # data corruption. Proceeding could add garbage to the pool.
    # Scenario: operator's machine crashed after LUKS format but before btrfs
    # device add completed.
    dev = "/dev/disk/by-id/virtio-disk4"
    machine.succeed(
        f"echo -n '{passphrase}' | cryptsetup luksFormat --batch-mode --key-file=- "
        f"--label braid-disk4 {luks_opts} {dev}"
    )
    machine.fail(add_cmd("disk4"))

    # Clean up: close mapper if open, wipe header so disk4 is fresh
    machine.execute("cryptsetup close braid-disk4 2>/dev/null || true")
    machine.succeed(f"dd if=/dev/zero of={dev} bs=1M count=4")

with subtest("Fresh disk4 added after cleanup"):
    # Intent: after cleaning up the rejected LUKS, a fresh disk can be added normally.
    # Why: proves the refusal didn't leave any state that blocks future adds.
    # Scenario: operator wipes the rejected disk and retries.
    machine.succeed(add_cmd("disk4"))
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "/dev/mapper/braid-disk4" in fi_show, f"braid-disk4 missing:\n{fi_show}"

# --- Phase 6: Fifth disk expands pool ---

with subtest("Fifth disk expands pool"):
    machine.succeed(add_cmd("disk5"))

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "/dev/mapper/braid-disk5" in fi_show, f"braid-disk5 missing:\n{fi_show}"
    devid_count = fi_show.count("devid")
    assert devid_count == 5, f"Expected 5 devices, got {devid_count}:\n{fi_show}"

with subtest("All data survived fifth disk addition"):
    content1 = machine.succeed("cat /mnt/storage/day1.txt").strip()
    content2 = machine.succeed("cat /mnt/storage/day2.txt").strip()
    assert content1 == "day one data", f"Expected 'day one data', got '{content1}'"
    assert content2 == "day two data", f"Expected 'day two data', got '{content2}'"

machine.shutdown()
