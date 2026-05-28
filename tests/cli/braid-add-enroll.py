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

import base64
import re
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
        f"braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 disk1=/dev/disk/by-id/virtio-disk1 --passphrase-stdin --yes"
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
        f"braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 disk2=/dev/disk/by-id/virtio-disk2 --passphrase-stdin --yes "
        f"--enroll /tmp >/tmp/add-enroll.out 2>/tmp/add-enroll.err"
    )
    add_err = machine.succeed("cat /tmp/add-enroll.err")
    # Principle 13: the in-add keyfile enroll path runs cryptsetup luksAddKey
    # (Argon2). A [wait] precedes it; an [ok] closes it.
    enroll_wait = "[wait] disk disk2: enrolling keyfile in slot 1..."
    enroll_ok = "[ok]   disk disk2: keyfile enrolled in slot 1"
    assert enroll_wait in add_err and enroll_ok in add_err, (
        f"expected enroll wait/ok pair, got: {add_err!r}"
    )
    assert add_err.find(enroll_wait) < add_err.find(enroll_ok), (
        f"enroll wait must precede enroll ok, got: {add_err!r}"
    )

    # Verify slot 1 is occupied on the new disk
    dump = machine.succeed(
        "cryptsetup luksDump --dump-json-metadata /dev/disk/by-id/virtio-disk2"
    )
    assert '"1"' in dump, f"slot 1 not found in luksDump for disk2: {dump}"

    # Regression: the on-disk header backup must capture slot 1 too.
    # A previous version backed up the header BEFORE running luksAddKey,
    # so the resulting `.luksheader` only contained slot 0. Restoring such
    # a backup would silently wipe keyfile-based unlock. Verify the backup
    # file produced during `braid add --enroll` contains slot 1 by
    # dumping it the same way we dump the live header above.
    backup_dump = machine.succeed(
        "cryptsetup luksDump --dump-json-metadata "
        "/var/lib/braid/luks-headers/braid-disk2.luksheader"
    )
    assert '"1"' in backup_dump, (
        "slot 1 not found in header backup for disk2 -- "
        f"backup was taken before luksAddKey: {backup_dump}"
    )

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
#   - `--dry-run`: stdout includes `[warn] Existing pool drives have a
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
        f"braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 disk3=/dev/disk/by-id/virtio-disk3 --passphrase-stdin --yes {extra}"
    )


with subtest("Test 4a: keyfile-asymmetry dry-run -> stdout [warn], stderr empty"):
    # Pool is mounted from Test 3, both disks have keyslot-1.
    machine.succeed("mountpoint -q /mnt/storage")

    machine.succeed(
        f"{add_cmd_disk3('--dry-run')} >/tmp/ka-stdout 2>/tmp/ka-stderr"
    )
    out = machine.succeed("cat /tmp/ka-stdout")
    err = machine.succeed("cat /tmp/ka-stderr")

    assert "[warn] Existing pool drives have a keyfile (keyslot-1)" in out, (
        "dry-run stdout must surface the keyfile-asymmetry Warn; got: {!r}".format(out)
    )
    assert "WARNING:" not in out, (
        "dry-run must NOT carry the legacy `WARNING:` prefix in the note body; got: {!r}".format(out)
    )
    assert err == "", (
        "dry-run stderr must be empty on success; got: {!r}".format(err)
    )

with subtest("Test 4b: failed keyfile probe dry-run -> stdout [warn], stderr empty"):
    # Intent: if keyfile-enrollment probing cannot inspect the existing
    # pool members, `braid add --dry-run` must surface the uncertainty as
    # a PreviewNote::Warn on stdout and keep stderr empty.
    # Why it exists: the LUKS helper used to print probe failures directly
    # to stderr. Dry-run success output is stdout-owned, so stderr leakage
    # broke scripts that treat dry-runs as clean previews.
    # Scenario: invoke the unwrapped braid binary with a PATH shim where
    # cryptsetup fails only `luksDump`; all other cryptsetup calls delegate
    # to the real binary.
    machine.succeed("mountpoint -q /mnt/storage")

    braid_wrapped_path = machine.succeed("readlink -f $(command -v braid)").strip()
    wrapper_source = machine.succeed(f"cat {braid_wrapped_path}")
    m = re.search(r'(/nix/store/[^"\s]+/bin/braid)(?!\-)', wrapper_source)
    assert m, f"could not locate unwrapped braid in wrapper:\n{wrapper_source}"
    unwrapped_braid = m.group(1)
    real_cryptsetup = machine.succeed("command -v cryptsetup").strip()

    wrapper_template = """#!/usr/bin/env bash
set -eu
if [ "${1:-}" = "luksDump" ]; then
    printf 'forced luksDump failure for add dry-run\\n' >&2
    exit 5
fi
exec __REAL_CRYPTSETUP__ "$@"
"""
    wrapper_script = wrapper_template.replace("__REAL_CRYPTSETUP__", real_cryptsetup)
    wrapper_b64 = base64.b64encode(wrapper_script.encode()).decode()
    machine.succeed(
        "rm -rf /tmp/wrap && mkdir -p /tmp/wrap && "
        f"printf '%s' {shlex.quote(wrapper_b64)} | base64 -d > /tmp/wrap/cryptsetup && "
        "chmod +x /tmp/wrap/cryptsetup"
    )

    (status, _) = machine.execute(
        "PATH=/tmp/wrap:$PATH "
        f"{unwrapped_braid} add disk3=/dev/disk/by-id/virtio-disk3 --dry-run "
        ">/tmp/probe-out 2>/tmp/probe-err"
    )
    assert status == 0, f"dry-run should succeed despite luksDump shim; exit {status}"
    out = machine.succeed("cat /tmp/probe-out")
    err = machine.succeed("cat /tmp/probe-err")

    assert "[warn] could not check keyfile enrollment" in out, (
        "dry-run stdout must surface the probe-failure Warn; got: {!r}".format(out)
    )
    assert "proceeding as if no keyfile is enrolled" in out, (
        "dry-run stdout must carry the canonical probe-failure suffix; got: {!r}".format(out)
    )
    assert "LUKS format /dev/disk/by-id/virtio-disk3" in out, (
        "dry-run stdout must still contain the normal preview steps; got: {!r}".format(out)
    )
    assert err == "", (
        "dry-run stderr must stay empty when the probe fails; got: {!r}".format(err)
    )

with subtest("Test 4c: keyfile-asymmetry real-run -> stderr canonical [warn] block"):
    # Intent: real-run keyfile-asymmetry now renders as the canonical
    # `[warn] Existing pool drives have a keyfile (keyslot-1) ...`
    # three-line block on stderr -- the SAME bytes dry-run produces on
    # stdout. The add-local `WARNING: ` legacy replay was removed;
    # plan-derived Warn notes route through the shared
    # `preview::render_notes_for_stderr` in both modes.
    # Why it exists: guards against a regression that reintroduces the
    # legacy `WARNING:` prefix on the Ok real-run path, producing two
    # different wordings for the same note across modes.
    # Scenario: pool is mounted from Test 3 with both disks carrying
    # keyslot-1; operator adds disk3 without --enroll.
    machine.succeed("mountpoint -q /mnt/storage")

    machine.succeed(
        f"{add_cmd_disk3()} >/tmp/rka-stdout 2>/tmp/rka-stderr"
    )
    err = machine.succeed("cat /tmp/rka-stderr")

    expected_block = (
        "[warn] Existing pool drives have a keyfile (keyslot-1) for auto-unlock,"
        " but the new drive will not.\n"
        "  Passphrase unlock still works, but the keyfile won't unlock the new drive"
        " until it's enrolled.\n"
        "  Fix: re-run with --enroll <dir>, or run `braid enroll <dir>` afterward.\n"
        "\n"
    )
    assert expected_block in err, (
        "real-run stderr must contain the canonical `[warn] ...`"
        " three-line block (with trailing blank line); got: {!r}".format(err)
    )
    assert "WARNING:" not in err, (
        "real-run stderr must NOT carry the legacy `WARNING:` prefix;"
        " got: {!r}".format(err)
    )

with subtest("Test 4d: returning slot0-only disk dry-run emits keyfile-asymmetry warn"):
    # Intent: re-adding a returning LUKS disk without slot 1 emits the same
    # keyfile-asymmetry dry-run warning as a fresh add.
    # Why it exists: the add planner used to warn only for fresh-format
    # targets, so returning disks could miss the auto-unlock advisory.
    # Scenario: disk3 was added without --enroll in Test 4c, then removed
    # without wiping its LUKS header; re-adding it should warn.
    machine.succeed("braid remove disk3 --yes")

    machine.succeed(
        f"{add_cmd_disk3('--dry-run')} >/tmp/rka-returning-stdout 2>/tmp/rka-returning-stderr"
    )
    out = machine.succeed("cat /tmp/rka-returning-stdout")
    err = machine.succeed("cat /tmp/rka-returning-stderr")

    assert "[warn] Existing pool drives have a keyfile (keyslot-1)" in out, (
        "returning-disk dry-run stdout must surface the keyfile-asymmetry Warn; got: {!r}".format(out)
    )
    assert err == "", (
        "returning-disk dry-run stderr must be empty on success; got: {!r}".format(err)
    )

machine.shutdown()
