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
        f"braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 {key}=/dev/disk/by-id/virtio-{key} --passphrase-stdin --yes"
    )


def close_all():
    machine.execute("umount /mnt/storage 2>/dev/null || true")
    for k in ["disk1", "disk2"]:
        machine.execute(f"cryptsetup close braid-{k} 2>/dev/null || true")


def assert_ordered_pair(text, first, second, context):
    assert first in text, f"missing {first!r} for {context}; got: {text!r}"
    assert second in text, f"missing {second!r} for {context}; got: {text!r}"
    assert text.find(first) < text.find(second), (
        f"{first!r} must precede {second!r} for {context}; got: {text!r}"
    )


# --- Setup: Create 2-disk RAID1 pool ---

with subtest("Setup: create 2-disk pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed("echo 'keyfile test data' > /mnt/storage/keytest.txt")
    machine.succeed("sync")

with subtest("Generate random keyfile"):
    machine.succeed("dd if=/dev/urandom of=/tmp/braid.key bs=4096 count=1 iflag=fullblock")
    machine.succeed("chmod 400 /tmp/braid.key")

# --- Test 1a: Dry-run before enrollment announces keyfile probes ---

with subtest("Test 1a: dry-run before enrollment announces keyfile probes"):
    machine.succeed("braid enroll /tmp --dry-run >/tmp/t1a.out 2>/tmp/t1a.err")
    t1a_out = machine.succeed("cat /tmp/t1a.out")
    t1a_err = machine.succeed("cat /tmp/t1a.err")
    for name in ("disk1", "disk2"):
        assert_ordered_pair(
            t1a_err,
            f"[wait] keyfile: checking against {name}...",
            f"[skip] keyfile: not yet enrolled on {name}",
            f"dry-run not-yet-enrolled probe for {name}",
        )
        expected_step = "enroll keyfile"
        expected_device = f"/dev/disk/by-id/virtio-{name}"
        assert expected_step in t1a_out and expected_device in t1a_out, (
            f"expected dry-run enroll step for {name}, got: {t1a_out!r}"
        )

# --- Test 1: Enroll keyfile into both disks ---

with subtest("Test 1: enroll keyfile into all pool disks"):
    pq = shlex.quote(passphrase)
    machine.succeed(
        f"printf '%s\\n' {pq} | braid enroll /tmp --passphrase-stdin "
        f">/tmp/t1.out 2>/tmp/t1.err"
    )

    # Behavioral wording lock (plan: "real-run wording unchanged"):
    # `plan_enrollment` emits the pre-apply `enroll: ...` line for
    # every candidate needing enrollment. A regression that hoisted
    # these into pre-passphrase notes, reworded them, or dropped
    # them entirely would silently change a user-visible stderr
    # string; we pin both lines byte-for-byte here.
    t1_err = machine.succeed("cat /tmp/t1.err")
    assert "[wait] passphrase: checking against disk1..." in t1_err, (
        f"expected passphrase verification wait line on stderr, got: {t1_err!r}"
    )
    assert_ordered_pair(
        t1_err,
        "[wait] passphrase: checking against disk1...",
        "[ok]   passphrase: accepted by disk1",
        "initial passphrase verification",
    )
    for name in ("disk1", "disk2"):
        marker = f"[wait] keyfile: checking against {name}..."
        skip_marker = f"[skip] keyfile: not yet enrolled on {name}"
        enroll_marker = f"enroll: {name} -- will add keyfile to slot 1"
        assert_ordered_pair(
            t1_err,
            marker,
            skip_marker,
            f"real-run not-yet-enrolled probe for {name}",
        )
        assert t1_err.find(skip_marker) < t1_err.find(enroll_marker), (
            f"keyfile skip line must precede enroll row for {name}, got: {t1_err!r}"
        )
    assert "enroll: disk1 -- will add keyfile to slot 1" in t1_err, (
        f"expected exact 'enroll: disk1 --' line on stderr, got: {t1_err!r}"
    )
    assert "enroll: disk2 -- will add keyfile to slot 1" in t1_err, (
        f"expected exact 'enroll: disk2 --' line on stderr, got: {t1_err!r}"
    )
    # Principle 13: a [wait] row precedes the cryptsetup luksAddKey call
    # (Argon2 derivation), closed by a paired [ok] success row.
    for name in ("disk1", "disk2"):
        enrolling_wait = f"[wait] disk {name}: enrolling keyfile in slot 1..."
        enrolled_ok = f"[ok]   disk {name}: keyfile enrolled in slot 1"
        assert enrolling_wait in t1_err, (
            f"expected enrolling wait line for {name}, got: {t1_err!r}"
        )
        assert enrolled_ok in t1_err, (
            f"expected enrolled ok line for {name}, got: {t1_err!r}"
        )
        assert t1_err.find(enrolling_wait) < t1_err.find(enrolled_ok), (
            f"enrolling wait must precede enrolled ok for {name}, got: {t1_err!r}"
        )

    # Verify slot 1 is occupied on both disks
    for dev in ["virtio-disk1", "virtio-disk2"]:
        dump = machine.succeed(f"cryptsetup luksDump --dump-json-metadata /dev/disk/by-id/{dev}")
        assert '"1"' in dump, f"slot 1 not found in luksDump for {dev}: {dump}"

# --- Test 1b: --dry-run reflects already-enrolled state ---
#
# Intent: verify `braid enroll --dry-run` renders a faithful preview
# on a pool whose keyfile is already enrolled -- both disks appear as
# per-disk Skip notes (`keyfile already enrolled`), no enroll/header
# backup steps are emitted, and blocking keyfile probes are announced
# on stderr per Principle 13.
#
# Why it exists: dry-run probes enrollment state pre-passphrase via
# the passphrase-free `verify_key_file` call (`cryptsetup open
# --test-passphrase --key-file`). Without this, dry-run silently
# overstated the work for the idempotent re-enroll case while the
# real run skipped via `plan_enrollment`'s `AlreadyEnrolled` branch
# -- contradicting decision-012's "intent CLI" promise that dry-run
# is a faithful preview. This subtest pins both the per-disk Skip
# wording and the canonical stderr probe rows.
#
# Scenario: 2-disk pool, both disks present and LUKS-formatted,
# keyfile already enrolled from Test 1.
with subtest("Test 1b: --dry-run reflects already-enrolled state"):
    machine.succeed("braid enroll /tmp --dry-run >/tmp/enroll.out 2>/tmp/enroll.err")
    out = machine.succeed("cat /tmp/enroll.out")
    err = machine.succeed("cat /tmp/enroll.err")
    expected_err = (
        "[wait] keyfile: checking against disk1...\n"
        "[ok]   keyfile: already enrolled on disk1\n"
        "[wait] keyfile: checking against disk2...\n"
        "[ok]   keyfile: already enrolled on disk2\n"
    )
    assert err == expected_err, (
        f"expected only canonical dry-run probe rows on stderr, got: {err!r}"
    )
    assert "enroll keyfile" not in out, (
        f"expected no enroll step on already-enrolled pool, got: {out!r}"
    )
    assert "[skip] disk disk1: keyfile already enrolled" in out, (
        f"expected disk1 skip note in preview, got: {out!r}"
    )
    assert "[skip] disk disk2: keyfile already enrolled" in out, (
        f"expected disk2 skip note in preview, got: {out!r}"
    )
    assert "nothing to do." in out, (
        f"expected 'nothing to do.' footer when both disks already enrolled, got: {out!r}"
    )

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
        f"printf '%s\\n' {pq} | braid enroll /tmp --passphrase-stdin "
        f">/tmp/t3.out 2>/tmp/t3.err"
    )

    # Principle 13: in the idempotent re-enroll path, the keyfile probe
    # wait row is closed by a canonical [ok] row per disk.
    t3_err = machine.succeed("cat /tmp/t3.err")
    for name in ("disk1", "disk2"):
        assert_ordered_pair(
            t3_err,
            f"[wait] keyfile: checking against {name}...",
            f"[ok]   keyfile: already enrolled on {name}",
            f"idempotent re-enroll for {name}",
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
# fails before emitting any keyfile already-enrolled or
# `enroll: <disk> -- will add keyfile to slot 1` line.
#
# Why it exists: those status lines are emitted by `plan_enrollment`
# only after passphrase verification succeeds. A regression that hoisted
# them into pre-passphrase planning would cause them to appear before the
# wrong-passphrase error -- misleading the user into thinking their
# enrollment partially succeeded. This subtest pins the no-leak behavior.
#
# Scenario: pool has keyfile enrolled on both disks (from Test 1);
# user fat-fingers the passphrase on a subsequent `braid enroll` run.
with subtest("Test 4b: wrong passphrase does not leak post-passphrase rows"):
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
    assert "[wait] passphrase: checking against disk1..." in err, (
        f"expected passphrase verification wait line before rejection, got: {err!r}"
    )
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
    assert "keyfile: already enrolled" not in err, (
        f"already-enrolled row leaked before wrong-passphrase error: {err!r}"
    )

# --- Test 4c: Real-run success path renders `skip:` plain on stderr ---
#
# Intent: verify that when a real-run `braid enroll` succeeds with
# at least one pool member absent, the accumulated skip note
# renders on stderr in the plain `skip: <name> not present`
# wording -- pre-passphrase, before the surviving candidate's
# canonical already-enrolled / `enroll:` status lines.
#
# Why it exists: the migration's "success-path real-run" coverage
# otherwise only asserts `enroll:` (Test 1) and canonical [ok]
# wording (Test 3); the exact plain `skip:` wording on a *successful* run
# has no behavioral anchor. Only the no-candidates failure path in
# `braid-enroll-generate.py` asserts plain skip wording today, and
# a regression that reworded the success-path skip render (or
# routed it to stdout) would not be caught there. This pins the
# contract: the same note body renders on stderr during a real-run
# that still finds at least one candidate.
#
# Scenario: disk1 + disk2 both enrolled (from Test 1). We edit
# pool.json to add a synthetic `disk3` member pointing at a by-id
# path that does not exist, making the Absent branch fire during
# discovery while the two real members remain as `AlreadyEnrolled`
# candidates. Pool.json is restored before Test 5 so the slot-
# conflict test sees its expected state.
with subtest("Test 4c: real-run success path renders plain `skip:` on stderr"):
    close_all()

    machine.succeed("cp /var/lib/braid/pool.json /tmp/pool.bak.json")
    machine.succeed(
        "jq '.disks.disk3 = {\"by_id\": \"/dev/disk/by-id/virtio-missing\"}' "
        "/var/lib/braid/pool.json > /tmp/pool.json && "
        "mv /tmp/pool.json /var/lib/braid/pool.json"
    )

    pq = shlex.quote(passphrase)
    machine.succeed(
        f"printf '%s\\n' {pq} | braid enroll /tmp --passphrase-stdin "
        f">/tmp/t4c.out 2>/tmp/t4c.err"
    )
    t4c_err = machine.succeed("cat /tmp/t4c.err")
    # Plain skip wording for the absent synthetic disk -- pins the
    # `render_notes_for_stderr(.., PerDiskStyle::Plain)` output shape.
    assert "skip: disk3 not present" in t4c_err, (
        f"expected plain 'skip: disk3 not present' on stderr, got: {t4c_err!r}"
    )
    # Sanity: surviving real members were still classified post-passphrase
    # as AlreadyEnrolled (they hold the keyfile from Test 1).
    for name in ("disk1", "disk2"):
        assert_ordered_pair(
            t4c_err,
            f"[wait] keyfile: checking against {name}...",
            f"[ok]   keyfile: already enrolled on {name}",
            f"absent-disk mixed real-run for {name}",
        )

    # Restore the real pool.json before Test 5 runs.
    machine.succeed("mv /tmp/pool.bak.json /var/lib/braid/pool.json")

# --- Test 4d: `skip: not LUKS-formatted` on real-run success path ---
#
# Intent: verify that when a real-run `braid enroll` succeeds with
# one pool member in the PresentNotLuks probe state, the
# accumulated skip note renders plain on stderr with the exact wording
# `skip: <name> not LUKS-formatted`, pre-passphrase, and the surviving
# candidates' canonical already-enrolled rows follow.
#
# Why it exists: Test 4c pins the plain-skip success-path wording
# for the Absent branch (`not present`); Test 5 in
# braid-enroll-generate.py pins `not LUKS-formatted` but only on
# the failure path. Neither anchors the `not LUKS-formatted` skip
# body on a *successful* real-run. A regression that reworded the
# not-LUKS-formatted message on the success path specifically
# (e.g. swapped message bodies between the two ConfigDiskState
# arms) would not be caught.
#
# Scenario: disk1 + disk2 both enrolled (from Test 1). We add a
# synthetic `disk3` entry to pool.json pointing at a 1 MiB regular
# file of zeros; probe_config_disk sees `fs.exists=true` and then
# `cryptsetup luksUUID` exits non-zero on the zero payload
# ("not a valid LUKS device"), classifying it as PresentNotLuks.
# Phase A re-checks the dry-run bracketed rendering for this
# branch under a minimal 2-surviving-candidates setup (less
# destructive than braid-enroll-generate.py's mixed-skip test).
# Phase B runs real-run and pins the plain stderr wording.
# Pool.json is restored before Test 5 so the slot-conflict test
# still sees its expected state.
with subtest("Test 4d: dry-run + real-run success path render `skip: not LUKS-formatted`"):
    close_all()

    # A zero-filled regular file is enough to trip `cryptsetup
    # luksUUID` into non-zero exit. We do not need a block device
    # -- cryptsetup reads the payload, fails to find the LUKS
    # header, and exits with "not a valid LUKS device". That path
    # is what classifies the disk as PresentNotLuks.
    machine.succeed("dd if=/dev/zero of=/tmp/fake-not-luks.bin bs=1M count=1")

    machine.succeed("cp /var/lib/braid/pool.json /tmp/pool.bak2.json")
    machine.succeed(
        "jq '.disks.disk3 = {\"by_id\": \"/tmp/fake-not-luks.bin\"}' "
        "/var/lib/braid/pool.json > /tmp/pool.json && "
        "mv /tmp/pool.json /var/lib/braid/pool.json"
    )

    # Phase A: dry-run -- bracketed skip on stdout, canonical probe rows on stderr.
    machine.succeed(
        "braid enroll /tmp --dry-run >/tmp/t4d.out 2>/tmp/t4d.err"
    )
    t4d_out = machine.succeed("cat /tmp/t4d.out")
    t4d_err = machine.succeed("cat /tmp/t4d.err")
    expected_t4d_err = (
        "[wait] keyfile: checking against disk1...\n"
        "[ok]   keyfile: already enrolled on disk1\n"
        "[wait] keyfile: checking against disk2...\n"
        "[ok]   keyfile: already enrolled on disk2\n"
    )
    assert t4d_err == expected_t4d_err, (
        f"expected only canonical dry-run probe rows on stderr, got: {t4d_err!r}"
    )
    assert "[skip] disk disk3: not LUKS-formatted\n" in t4d_out, (
        f"expected bracketed non-LUKS skip on stdout, got: {t4d_out!r}"
    )
    assert "\x1b[" not in t4d_out, (
        f"dry-run stdout must be plain without a TTY, got: {t4d_out!r}"
    )

    # Phase B: real-run success -- plain skip on stderr, surviving
    # candidates classified AlreadyEnrolled (keyfile from Test 1).
    pq = shlex.quote(passphrase)
    machine.succeed(
        f"printf '%s\\n' {pq} | braid enroll /tmp --passphrase-stdin "
        f">/tmp/t4d.rout 2>/tmp/t4d.rerr"
    )
    t4d_rerr = machine.succeed("cat /tmp/t4d.rerr")
    assert "skip: disk3 not LUKS-formatted" in t4d_rerr, (
        f"expected plain 'skip: disk3 not LUKS-formatted' on stderr, got: {t4d_rerr!r}"
    )
    for name in ("disk1", "disk2"):
        assert_ordered_pair(
            t4d_rerr,
            f"[wait] keyfile: checking against {name}...",
            f"[ok]   keyfile: already enrolled on {name}",
            f"not-LUKS mixed real-run for {name}",
        )

    # Restore real pool.json and clean up the synthetic device.
    machine.succeed("mv /tmp/pool.bak2.json /var/lib/braid/pool.json")
    machine.succeed("rm /tmp/fake-not-luks.bin")

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

# --- Test 5b: Preflight detects divergent passphrase before any enrollment ---
#
# Intent: When a disk's passphrase has been changed out-of-band so that
# pool members no longer share a single passphrase, `braid enroll` fails
# during planning -- before any disk is mutated.
#
# Why it exists: The two-phase enroll refactor's stated guarantee is "no
# partial mutation on preflight failure". Test 5 covers the slot-1
# conflict path. Before this test was added, the planner verified the
# passphrase only against the first candidate, so a divergent passphrase
# on disk2 would pass planning, disk1 would be enrolled, and then the
# apply phase would fail on disk2's `luksAddKey` -- leaving exactly the
# partial-mutation state the refactor exists to prevent. This pins the
# per-disk passphrase verify in the planner against real cryptsetup.
#
# Scenario: A user (or a misbehaving script) ran `cryptsetup
# luksChangeKey` against one disk in the pool. The next `braid enroll`
# must reject the divergence and leave the pool untouched.

with subtest("Test 5b: preflight detects divergent passphrase before any mutation"):
    close_all()

    # Slot-1 cleanup must be idempotent: Test 5 left disk2 slot 1
    # occupied by /tmp/conflict.key but disk1 slot 1 already empty.
    # `cryptsetup luksKillSlot` rejects an inactive slot, so the kill
    # must be tolerant.
    for dev in ["virtio-disk1", "virtio-disk2"]:
        machine.execute(
            f"cryptsetup luksKillSlot --batch-mode /dev/disk/by-id/{dev} 1 "
            "2>/dev/null || true"
        )

    pq = shlex.quote(passphrase)

    # Confirm starting state: both disks accept the original passphrase
    # (slot 0 holds it on both), and slot 1 is empty on both.
    for dev in ["virtio-disk1", "virtio-disk2"]:
        machine.succeed(
            f"printf '%s\\n' {pq} | "
            f"cryptsetup open --test-passphrase /dev/disk/by-id/{dev}"
        )

    # Diverge disk2's passphrase. `--key-slot 0` is required: without it,
    # `luksChangeKey` may allocate a free slot (slot 1 is empty on this
    # VM) for the new key and leave the old one in slot 0, silently
    # turning this into a slot-conflict test rather than a divergent-
    # passphrase test.
    new_pass = "differentpassphrase"
    npq = shlex.quote(new_pass)
    machine.succeed(
        f"printf '%s\\n%s\\n' {pq} {npq} "
        "| cryptsetup luksChangeKey --key-slot 0 --batch-mode "
        "/dev/disk/by-id/virtio-disk2"
    )

    # Verify the divergence is real: disk1 still accepts the original
    # passphrase, disk2 does not. Guards against `luksChangeKey` having
    # silently no-opped or operated on a different slot.
    machine.succeed(
        f"printf '%s\\n' {pq} | "
        "cryptsetup open --test-passphrase /dev/disk/by-id/virtio-disk1"
    )
    machine.fail(
        f"printf '%s\\n' {pq} | "
        "cryptsetup open --test-passphrase /dev/disk/by-id/virtio-disk2"
    )

    # Run braid enroll. Use `machine.execute` (not `succeed`/`fail`) so
    # we can capture combined stdout+stderr regardless of exit status.
    status, output = machine.execute(
        f"printf '%s\\n' {pq} | braid enroll /tmp --passphrase-stdin 2>&1"
    )
    assert status != 0, (
        "expected nonzero exit on divergent passphrase; got "
        f"status={status}, output={output!r}"
    )
    assert "wrong passphrase" in output, (
        f"expected 'wrong passphrase' in output, got: {output!r}"
    )
    assert "disk2" in output, (
        f"expected 'disk2' to be named in error, got: {output!r}"
    )

    # Disk1 slot 1 must still be empty -- planning aborted before any
    # mutation. If the per-disk passphrase verify regressed, disk1 would
    # have been enrolled before disk2's apply-time failure.
    machine.fail(
        "cryptsetup open --test-passphrase --key-file /tmp/braid.key "
        "/dev/disk/by-id/virtio-disk1"
    )

    # Revert disk2's slot 0 to the original passphrase so the per-VM
    # state stays consistent if any future test is appended after 5b.
    machine.succeed(
        f"printf '%s\\n%s\\n' {npq} {pq} "
        "| cryptsetup luksChangeKey --key-slot 0 --batch-mode "
        "/dev/disk/by-id/virtio-disk2"
    )

machine.shutdown()
