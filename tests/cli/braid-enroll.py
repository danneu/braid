# Test: braid-enroll
#
# Intent: Verify that `braid enroll` enrolls a binary keyfile into
# LUKS slot 1 on all pool disks, and that `braid unlock --key-file` can
# subsequently open them.
#
# Why it exists: The keyfile enrollment path uses different cryptsetup
# semantics than passphrase (raw bytes, explicit slot, no PBKDF). If
# enrollment silently fails or targets the wrong slot, auto-unlock breaks
# at 3 AM when nobody is watching.
#
# Scenario: 2-disk RAID1 pool. Generate 4096-byte random keyfile. Enroll
# into both disks. Lock pool. Unlock with keyfile. Verify data intact.
# Re-enroll (idempotent). Verify passphrase path still works.

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
        f"braid add {key}=/dev/disk/by-id/virtio-{key} --passphrase-stdin --yes"
    )


def close_all():
    machine.execute("umount /mnt/storage 2>/dev/null || true")
    for k in ["disk1", "disk2"]:
        machine.execute(f"cryptsetup close braid-{k} 2>/dev/null || true")


# --- Setup: Create 2-disk RAID1 pool ---

with subtest("Setup: create 2-disk pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed("echo 'keyfile test data' > /mnt/storage/keytest.txt")
    machine.succeed("sync")

with subtest("Generate random keyfile"):
    machine.succeed("dd if=/dev/urandom of=/tmp/braid.key bs=4096 count=1 iflag=fullblock")
    machine.succeed("chmod 400 /tmp/braid.key")

# --- Test 1: Enroll keyfile into both disks ---

with subtest("Test 1: enroll keyfile into all pool disks"):
    pq = shlex.quote(passphrase)
    machine.succeed(
        f"printf '%s\\n' {pq} | braid enroll /tmp --passphrase-stdin"
    )

    # Verify slot 1 is occupied on both disks
    for dev in ["virtio-disk1", "virtio-disk2"]:
        dump = machine.succeed(f"cryptsetup luksDump --dump-json-metadata /dev/disk/by-id/{dev}")
        assert '"1"' in dump, f"slot 1 not found in luksDump for {dev}: {dump}"

# --- Test 1b: --dry-run writes preview to stdout, stderr is empty ---
#
# Intent: verify `braid enroll --dry-run` renders exactly one Preview
# to stdout and leaves stderr empty on the success path -- the
# project-wide rule introduced in the `Preview`-migration plan.
#
# Why it exists: the dry-run migration moved pre-passphrase discovery
# notes (`skip: X not present`, `skip: X not LUKS-formatted`) off
# stderr into the Preview on stdout. A regression that leaks those
# notes back to stderr would silently break the "successful dry-run
# = empty stderr" contract. This subtest runs while all pool members
# are present LUKS disks, so the Preview is steps-only and stderr
# must be empty byte-for-byte.
#
# Scenario: 2-disk pool, both disks present and LUKS-formatted,
# keyfile already enrolled from Test 1 (dry-run does not depend on
# slot 1 state -- planner classification is post-passphrase and
# bypassed in dry-run).
with subtest("Test 1b: --dry-run writes preview to stdout, stderr empty"):
    machine.succeed("braid enroll /tmp --dry-run >/tmp/enroll.out 2>/tmp/enroll.err")
    out = machine.succeed("cat /tmp/enroll.out")
    err = machine.succeed("cat /tmp/enroll.err")
    assert err == "", f"unexpected stderr on successful --dry-run: {err!r}"
    assert "enroll keyfile" in out, f"expected enroll step in stdout, got: {out!r}"

# --- Test 2: Lock, then unlock with keyfile ---

with subtest("Test 2: unlock with keyfile"):
    close_all()

    machine.fail("mountpoint -q /mnt/storage")

    machine.succeed("braid unlock --key-file /tmp/braid.key")

    machine.succeed("mountpoint -q /mnt/storage")
    for k in ["disk1", "disk2"]:
        machine.succeed(f"test -e /dev/mapper/braid-{k}")

    content = machine.succeed("cat /mnt/storage/keytest.txt").strip()
    assert content == "keyfile test data", f"Expected 'keyfile test data', got '{content}'"

# --- Test 3: Re-enroll is idempotent ---

with subtest("Test 3: re-enroll is idempotent"):
    pq = shlex.quote(passphrase)
    machine.succeed(
        f"printf '%s\\n' {pq} | braid enroll /tmp --passphrase-stdin"
    )

# --- Test 4: Passphrase still works after keyfile enrollment ---

with subtest("Test 4: passphrase still works"):
    close_all()

    pq = shlex.quote(passphrase)
    machine.succeed(f"printf '%s\\n' {pq} | braid unlock --passphrase-stdin")

    machine.succeed("mountpoint -q /mnt/storage")
    content = machine.succeed("cat /mnt/storage/keytest.txt").strip()
    assert content == "keyfile test data", f"Expected 'keyfile test data', got '{content}'"

# --- Test 4b: Wrong passphrase does not leak post-passphrase status lines ---
#
# Intent: verify a real-run `braid enroll` with a wrong passphrase
# fails before emitting any `ok: <disk> -- keyfile already enrolled`
# or `enroll: <disk> -- will add keyfile to slot 1` line.
#
# Why it exists: those status lines are emitted by `plan_enrollment`
# only after `verify_first_candidate_passphrase` succeeds. The
# `Preview` migration scoped these lines out of notes (they stay as
# direct `eprintln!`s inside `plan_enrollment`), and a regression
# that hoisted them into pre-passphrase planning would cause them
# to appear before the wrong-passphrase error -- misleading the
# user into thinking their enrollment partially succeeded. This
# subtest pins the no-leak behavior.
#
# Scenario: pool has keyfile enrolled on both disks (from Test 1);
# user fat-fingers the passphrase on a subsequent `braid enroll` run.
with subtest("Test 4b: wrong passphrase does not leak ok:/enroll: status lines"):
    wrongpass = "wrongpassphrase"
    wpq = shlex.quote(wrongpass)
    # The NixOS test driver wraps commands with `set -euo pipefail`, so
    # a direct `cmd; echo $? > rc` would abort after cmd fails. Using
    # `|| rc=$?` captures the nonzero exit without tripping `set -e`.
    machine.succeed(
        f"rc=0; printf '%s\\n' {wpq} | braid enroll /tmp --passphrase-stdin "
        f">/tmp/wp.out 2>/tmp/wp.err || rc=$?; echo $rc > /tmp/wp.rc"
    )
    rc = machine.succeed("cat /tmp/wp.rc").strip()
    err = machine.succeed("cat /tmp/wp.err")
    assert rc != "0", f"expected nonzero exit on wrong passphrase; got rc={rc}, err={err!r}"
    assert "wrong passphrase" in err, f"expected wrong-passphrase error, got: {err!r}"
    for line in err.splitlines():
        stripped = line.strip()
        assert not stripped.startswith("ok:"), (
            f"ok: status line leaked before wrong-passphrase error: {line!r}\n"
            f"full stderr: {err!r}"
        )
        assert not stripped.startswith("enroll:"), (
            f"enroll: status line leaked before wrong-passphrase error: {line!r}\n"
            f"full stderr: {err!r}"
        )

# --- Test 5: Preflight detects slot conflict before any enrollment ---

with subtest("Test 5: preflight detects slot-1 conflict before any mutation"):
    # This is the regression test for the two-phase refactor. The old code
    # enrolled disk1 (slot 1 empty) before discovering that disk2 had a
    # slot-1 conflict, leaving the pool partially mutated. The new code
    # detects the conflict during planning and fails before any mutation.
    close_all()

    # Remove the keyfile from both disks so slot 1 is empty again
    for dev in ["virtio-disk1", "virtio-disk2"]:
        machine.succeed(f"cryptsetup luksKillSlot --batch-mode /dev/disk/by-id/{dev} 1")

    # Put an unknown key into disk2's slot 1 (simulates an external tool
    # or manual cryptsetup having claimed the slot)
    machine.succeed("dd if=/dev/urandom of=/tmp/conflict.key bs=32 count=1 iflag=fullblock")
    pq = shlex.quote(passphrase)
    machine.succeed(
        f"printf '%s\\n' {pq} | "
        f"cryptsetup luksAddKey --key-slot 1 /dev/disk/by-id/virtio-disk2 /tmp/conflict.key"
    )

    # Verify disk1 slot 1 is empty (the keyfile does not work on disk1)
    machine.fail(
        "cryptsetup open --test-passphrase --key-file /tmp/braid.key /dev/disk/by-id/virtio-disk1"
    )

    # Try to enroll — should fail due to slot-1 conflict on disk2
    machine.fail(
        f"printf '%s\\n' {pq} | braid enroll /tmp --passphrase-stdin"
    )

    # Verify disk1's slot 1 is STILL empty — preflight prevented mutation.
    # If the old (non-preflight) code were running, disk1 would have been
    # enrolled before the disk2 conflict was detected.
    machine.fail(
        "cryptsetup open --test-passphrase --key-file /tmp/braid.key /dev/disk/by-id/virtio-disk1"
    )

machine.shutdown()
