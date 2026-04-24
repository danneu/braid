# Test: braid-enroll-generate
#
# Intent: Verify that `braid enroll --generate` atomically creates a
# 4096-byte keyfile with mode 400, enrolls it into all pool disks, and that
# generated keyfile can unlock the pool. Also verifies --generate refuses to
# overwrite an existing keyfile.
#
# Why it exists: The --generate flag replaces the manual dd/chmod workflow.
# If the keyfile is created before preflight validation (e.g., wrong
# passphrase), a useless keyfile is left behind. The two-phase approach
# (validate first, generate only on success) prevents this.
#
# Scenario: 2-disk RAID1 pool. --generate creates keyfile and enrolls.
# Lock, unlock with generated keyfile. --generate refuses overwrite.
# Slot conflict prevents keyfile creation.

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
    machine.succeed("echo 'generate test data' > /mnt/storage/gentest.txt")
    machine.succeed("sync")

# --- Test 1: --generate creates keyfile and enrolls ---

with subtest("Test 1: --generate creates keyfile and enrolls into all disks"):
    machine.succeed("mkdir -p /tmp/usb")
    pq = shlex.quote(passphrase)
    machine.succeed(
        f"printf '%s\\n' {pq} | braid enroll /tmp/usb --generate --passphrase-stdin"
    )

    # Verify keyfile exists with correct size and permissions
    machine.succeed("test -f /tmp/usb/braid.key")
    size = machine.succeed("stat -c %s /tmp/usb/braid.key").strip()
    assert size == "4096", f"Expected keyfile size 4096, got {size}"
    mode = machine.succeed("stat -c %a /tmp/usb/braid.key").strip()
    assert mode == "400", f"Expected mode 400, got {mode}"

    # Verify slot 1 is occupied on both disks
    for dev in ["virtio-disk1", "virtio-disk2"]:
        dump = machine.succeed(f"cryptsetup luksDump --dump-json-metadata /dev/disk/by-id/{dev}")
        assert '"1"' in dump, f"slot 1 not found in luksDump for {dev}: {dump}"

# --- Test 2: Lock, then unlock with generated keyfile ---

with subtest("Test 2: unlock with generated keyfile"):
    close_all()

    machine.fail("mountpoint -q /mnt/storage")

    machine.succeed("braid unlock --key-file /tmp/usb/braid.key")

    machine.succeed("mountpoint -q /mnt/storage")
    content = machine.succeed("cat /mnt/storage/gentest.txt").strip()
    assert content == "generate test data", f"Expected 'generate test data', got '{content}'"

# --- Test 3: --generate refuses to overwrite existing keyfile ---

with subtest("Test 3: --generate refuses overwrite"):
    pq = shlex.quote(passphrase)
    machine.fail(
        f"printf '%s\\n' {pq} | braid enroll /tmp/usb --generate --passphrase-stdin"
    )

# --- Test 4: Slot conflict prevents keyfile creation ---

with subtest("Test 4: slot conflict prevents keyfile creation"):
    close_all()

    # Remove keyfile from both disks and delete existing keyfile
    for dev in ["virtio-disk1", "virtio-disk2"]:
        machine.succeed(f"cryptsetup luksKillSlot --batch-mode /dev/disk/by-id/{dev} 1")
    machine.succeed("rm /tmp/usb/braid.key")

    # Put an unknown key into disk2's slot 1
    machine.succeed("dd if=/dev/urandom of=/tmp/conflict.key bs=32 count=1 iflag=fullblock")
    pq = shlex.quote(passphrase)
    machine.succeed(
        f"printf '%s\\n' {pq} | "
        f"cryptsetup luksAddKey --key-slot 1 /dev/disk/by-id/virtio-disk2 /tmp/conflict.key"
    )

    # --generate should fail due to slot conflict, and keyfile must NOT be created
    machine.fail(
        f"printf '%s\\n' {pq} | braid enroll /tmp/usb --generate --passphrase-stdin"
    )

    # Verify keyfile was NOT created (preflight prevented generation)
    machine.fail("test -f /tmp/usb/braid.key")

# --- Test 5: No-candidates preserved-context failure ---
#
# Intent: verify that when every pool member is non-LUKS, a real-run
# `braid enroll` prints each accumulated `skip: <name> not LUKS-
# formatted` line to stderr *before* the `no present LUKS disks
# found...` validation error -- the preserved-context failure
# contract for the Shape A `enroll` migration.
#
# Why it exists: the `Preview` migration converted today's direct
# `eprintln!("skip: ...")` calls to `PreviewNote::PerDisk { Skip }`
# entries accumulated on the `EnrollPlanReport`. On the `Err` branch
# the cmd wrapper renders those notes to stderr before propagating
# the error. A regression that dropped the notes on failure -- or
# re-ordered them after the error -- would silently strip the
# user-visible discovery context that explains *why* no candidates
# were found.
#
# Scenario: after Test 4, both disks are LUKS-formatted with slot-1
# in unusual states. We wipe both LUKS headers so discovery sees
# both disks as PresentNotLuks, producing zero candidates. The
# destructive wipefs is safe here -- this is the last subtest, and
# the VM is torn down on shutdown.
with subtest("Test 5: no-candidates preserved-context failure"):
    close_all()

    # Wipe the LUKS headers so both disks become "not LUKS-formatted"
    # from braid's point of view. --force is required because wipefs
    # otherwise refuses to touch a LUKS-formatted block device.
    for dev in ["virtio-disk1", "virtio-disk2"]:
        machine.succeed(f"wipefs --all --force /dev/disk/by-id/{dev}")

    machine.execute("rm -f /tmp/usb/braid.key")

    pq = shlex.quote(passphrase)
    # NixOS test driver uses `set -euo pipefail`; capture the expected
    # nonzero exit via `|| rc=$?` instead of a bare `; echo $?`.
    machine.succeed(
        f"rc=0; printf '%s\\n' {pq} | braid enroll /tmp/usb --generate --passphrase-stdin "
        f">/tmp/noc.out 2>/tmp/noc.err || rc=$?; echo $rc > /tmp/noc.rc"
    )
    rc = machine.succeed("cat /tmp/noc.rc").strip()
    err = machine.succeed("cat /tmp/noc.err")
    out = machine.succeed("cat /tmp/noc.out")
    assert rc != "0", f"expected nonzero exit on no-candidates; got rc={rc}"
    assert out == "", f"stdout must be empty on failure path, got: {out!r}"
    # Each membership disk accumulates a plain skip line, in iteration order.
    assert "skip: disk1 not LUKS-formatted" in err, (
        f"expected plain skip line for disk1, got: {err!r}"
    )
    assert "skip: disk2 not LUKS-formatted" in err, (
        f"expected plain skip line for disk2, got: {err!r}"
    )
    assert "no present LUKS disks" in err, (
        f"expected no-candidates validation error, got: {err!r}"
    )
    # Ordering contract: both skip lines precede the validation error.
    d1_idx = err.find("skip: disk1 not LUKS-formatted")
    d2_idx = err.find("skip: disk2 not LUKS-formatted")
    err_idx = err.find("no present LUKS disks")
    assert d1_idx != -1 and d2_idx != -1 and err_idx != -1, (
        f"missing expected line(s) in stderr: {err!r}"
    )
    assert d1_idx < err_idx, (
        f"disk1 skip line must precede error; got:\n{err!r}"
    )
    assert d2_idx < err_idx, (
        f"disk2 skip line must precede error; got:\n{err!r}"
    )
    # Keyfile must not have been created on the failure path.
    machine.fail("test -f /tmp/usb/braid.key")

machine.shutdown()
