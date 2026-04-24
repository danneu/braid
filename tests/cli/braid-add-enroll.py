# Test: braid-add-enroll
#
# Intent: Verify that `braid add --enroll` enrolls the keyfile
# into the new disk as part of the add operation, and that the disk is
# unlockable with both passphrase and keyfile afterward.
#
# Why it exists: The --enroll flag on add/replace wires
# enrollment into the format path, reusing the passphrase already in
# scope. If the passphrase handoff from luks_format() to
# enroll_key_file() is wrong (e.g., passphrase consumed/dropped),
# enrollment silently fails even though standalone enroll and
# unlock --key-file tests pass.
#
# Scenario: 1-disk pool created without keyfile. Generate keyfile.
# Add a second disk with --enroll. Verify new disk has keyfile
# in slot 1. Lock pool. Unlock with keyfile. Verify passphrase still
# works on both disks.

import shlex

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def close_all():
    machine.execute("umount /mnt/storage 2>/dev/null || true")
    for k in ["disk1", "disk2"]:
        machine.execute(f"cryptsetup close braid-{k} 2>/dev/null || true")


# --- Setup ---

with subtest("Setup: create 1-disk pool without keyfile"):
    pq = shlex.quote(passphrase)
    machine.succeed(
        f"printf '%s\\n' {pq} | "
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid add disk1=/dev/disk/by-id/virtio-disk1 --passphrase-stdin --yes"
    )
    machine.succeed("echo 'add-enroll test' > /mnt/storage/test.txt")
    machine.succeed("sync")

with subtest("Generate random keyfile"):
    machine.succeed("dd if=/dev/urandom of=/tmp/braid.key bs=4096 count=1 iflag=fullblock")
    machine.succeed("chmod 400 /tmp/braid.key")

# --- Test 1: Add second disk with --enroll ---

with subtest("Test 1: add disk2 with --enroll"):
    pq = shlex.quote(passphrase)
    machine.succeed(
        f"printf '%s\\n' {pq} | "
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid add disk2=/dev/disk/by-id/virtio-disk2 --passphrase-stdin --yes "
        f"--enroll /tmp"
    )

    # Verify slot 1 is occupied on the new disk
    dump = machine.succeed(
        "cryptsetup luksDump --dump-json-metadata /dev/disk/by-id/virtio-disk2"
    )
    assert '"1"' in dump, f"slot 1 not found in luksDump for disk2: {dump}"

# --- Test 2: Unlock with keyfile (only disk2 has it) ---

with subtest("Test 2: keyfile can open disk2"):
    close_all()

    # Enroll disk1 too so full pool unlocks with keyfile
    pq = shlex.quote(passphrase)
    # Reopen disk1 first to enroll it
    machine.succeed(
        f"printf '%s\\n' {pq} | braid enroll /tmp --passphrase-stdin"
    )

    close_all()
    machine.succeed("braid unlock --key-file /tmp/braid.key")
    machine.succeed("mountpoint -q /mnt/storage")

# --- Test 3: Passphrase still works on both disks ---

with subtest("Test 3: passphrase still works"):
    close_all()
    pq = shlex.quote(passphrase)
    machine.succeed(f"printf '%s\\n' {pq} | braid unlock --passphrase-stdin")
    machine.succeed("mountpoint -q /mnt/storage")
    content = machine.succeed("cat /mnt/storage/test.txt").strip()
    assert content == "add-enroll test", f"Expected 'add-enroll test', got '{content}'"

# --- Test 4: keyfile-asymmetry warning routing (dry-run vs real-run) ---
#
# Intent: `braid add disk3` on a pool where the existing drives carry a
# keyfile (keyslot-1), but the add omits `--enroll`, must surface the
# keyfile-asymmetry diagnostic on the correct stream for each mode:
#   - `--dry-run`: stdout includes `[warn]  Existing pool drives have a
#     keyfile (keyslot-1) ...`; stderr is empty.
#   - real-run: stderr contains the exact legacy three-line `WARNING:`
#     block (including the trailing blank line), byte-for-byte.
#
# Why it exists: PR 7 moves the legacy `WARNING:` eprintln! from a raw
# stderr write into a `PreviewNote::Warn` whose body is shared between
# dry-run and real-run. A regression that left `WARNING:` baked into the
# note body, dropped the trailing blank line, or routed the dry-run
# diagnostic back to stderr would still exit 0 and slip past the
# existing --enroll coverage.
#
# Scenario: after Test 3 the pool has both disk1 and disk2 with
# keyslot-1 populated. Operator wants to add disk3, forgets `--enroll`.

def add_cmd_disk3(extra=""):
    pq = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {pq} | "
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid add disk3=/dev/disk/by-id/virtio-disk3 --passphrase-stdin --yes {extra}"
    )


with subtest("Test 4a: keyfile-asymmetry dry-run -> stdout [warn], stderr empty"):
    # Pool is mounted from Test 3, both disks have keyslot-1.
    machine.succeed("mountpoint -q /mnt/storage")

    machine.succeed(
        f"{add_cmd_disk3('--dry-run')} >/tmp/ka-stdout 2>/tmp/ka-stderr"
    )
    out = machine.succeed("cat /tmp/ka-stdout")
    err = machine.succeed("cat /tmp/ka-stderr")

    assert "[warn]  Existing pool drives have a keyfile (keyslot-1)" in out, (
        "dry-run stdout must surface the keyfile-asymmetry Warn; got: {!r}".format(out)
    )
    assert "WARNING:" not in out, (
        "dry-run must NOT carry the legacy `WARNING:` prefix in the note body; got: {!r}".format(out)
    )
    assert err == "", (
        "dry-run stderr must be empty on success; got: {!r}".format(err)
    )

with subtest("Test 4b: keyfile-asymmetry real-run -> stderr has exact WARNING block"):
    # Real-run needs a fresh stderr fixture, so close + remount the pool
    # once to clear state, then add disk3 for real. The pool must stay
    # mounted with both disks open so pool_has_keyfile_enrollment sees
    # keyslot-1 on the live pool member.
    machine.succeed("mountpoint -q /mnt/storage")

    # Pipe stderr to a file; stdout discarded. Expect success.
    machine.succeed(
        f"{add_cmd_disk3()} >/tmp/rka-stdout 2>/tmp/rka-stderr"
    )
    err = machine.succeed("cat /tmp/rka-stderr")

    expected_block = (
        "WARNING: Existing pool drives have a keyfile (keyslot-1) for auto-unlock,"
        " but the new drive will not.\n"
        "  Passphrase unlock still works, but the keyfile won't unlock the new drive"
        " until it's enrolled.\n"
        "  Fix: re-run with --enroll <dir>, or run `braid enroll <dir>` afterward.\n"
        "\n"
    )
    assert expected_block in err, (
        "real-run stderr must contain the exact legacy 3-line WARNING block"
        " (with trailing blank line); got: {!r}".format(err)
    )

machine.shutdown()
