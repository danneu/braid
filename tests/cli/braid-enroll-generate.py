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

# --- Test 5: dry-run + real-run surface mixed skip notes (both variants) ---
#
# Intent: verify that discovery skip notes render correctly on BOTH
# the dry-run success path (bracketed on stdout, stderr empty) and
# the real-run no-candidates failure path (plain on stderr,
# preceding the validation error). Covers both skip variants: the
# `PresentNotLuks` branch via wipefs, and the `Absent` branch via a
# pool.json edit pointing disk2 at a nonexistent by-id path.
#
# Why it exists: the `Preview` migration turned today's direct
# `eprintln!("skip: X not present")` and
# `eprintln!("skip: X not LUKS-formatted")` lines into a single
# `PreviewNote::PerDisk { Skip }` shape. Two regressions must be
# caught here:
#   1. Dry-run stream routing: the skip notes must appear on stdout
#      (via `Preview::render`, bracketed), not stderr. A regression
#      that left them on stderr would silently violate the project-
#      wide "successful --dry-run = empty stderr" rule.
#   2. Preserved-context failure: on the no-candidates Err branch,
#      the same notes must render on stderr (via
#      `render_notes_for_stderr(.., Plain)`) *before* the validation
#      error, preserving today's stderr ordering byte-for-byte.
#
# Scenario: after Test 4, both disks are LUKS-formatted with slot-1
# in unusual states. We wipe disk1's LUKS header (PresentNotLuks)
# and rewrite pool.json so disk2 points at a fabricated by-id path
# that does not exist (Absent). This produces zero candidates --
# dry-run renders the two skip notes + nothing-to-do fallback;
# real-run renders the two skip notes + validation error. The
# destructive state is safe here -- this is the last subtest, and
# the VM is torn down on shutdown.
with subtest("Test 5: dry-run + real-run surface mixed skip notes"):
    close_all()

    # Arrange three membership entries of different probe-state flavors:
    #   disk1 -- PresentNotLuks via `wipefs --all --force` (requires
    #            --force because wipefs refuses a LUKS-formatted device).
    #   disk2 -- PresentLuks, the surviving candidate that keeps
    #            plan_enroll on the Ok branch so the dry-run preview
    #            actually renders (a zero-candidate plan returns Err
    #            in plan_enroll -- there is no "successful dry-run
    #            with zero steps" once we drop to zero candidates).
    #   disk3 -- Absent via a pool.json edit pointing its by_id at a
    #            path udev never populated. probe_config_disk hits
    #            fs.exists=false and returns ConfigDiskState::Absent.
    # This setup gives both skip variants (not LUKS-formatted + not
    # present) on the success path, which is what the dry-run Preview
    # renders in bracketed form.
    machine.succeed("wipefs --all --force /dev/disk/by-id/virtio-disk1")
    machine.succeed(
        "jq '.disks.disk3 = {\"by_id\": \"/dev/disk/by-id/virtio-missing\"}' "
        "/var/lib/braid/pool.json > /tmp/pool.json && "
        "mv /tmp/pool.json /var/lib/braid/pool.json"
    )
    machine.execute("rm -f /tmp/usb/braid.key")

    # Phase A: dry-run success -- both bracketed skip notes on stdout,
    # surviving disk2 enroll step also on stdout, stderr empty.
    machine.succeed(
        "braid enroll /tmp/usb --generate --dry-run "
        ">/tmp/mx.out 2>/tmp/mx.err"
    )
    mx_out = machine.succeed("cat /tmp/mx.out")
    mx_err = machine.succeed("cat /tmp/mx.err")
    assert mx_err == "", (
        f"successful --dry-run must leave stderr empty, got: {mx_err!r}"
    )
    # Bracketed shape is `[skip]  disk: <name:<10>><message>\n` --
    # matches `format_per_disk_line(.., PerDiskStyle::Bracketed)`.
    assert "[skip]  disk: disk1     not LUKS-formatted\n" in mx_out, (
        f"expected bracketed non-LUKS skip for disk1 on stdout, got: {mx_out!r}"
    )
    assert "[skip]  disk: disk3     not present\n" in mx_out, (
        f"expected bracketed absent skip for disk3 on stdout, got: {mx_out!r}"
    )
    # Sanity: the surviving candidate (disk2) contributes a step.
    assert "enroll keyfile" in mx_out, (
        f"expected enroll step for disk2 on stdout, got: {mx_out!r}"
    )

    # Phase B: drop to zero candidates, real-run -- preserved-context
    # failure. All three skip variants render plain on stderr, then
    # the validation error.
    machine.succeed("wipefs --all --force /dev/disk/by-id/virtio-disk2")
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
    assert "skip: disk1 not LUKS-formatted" in err, (
        f"expected plain non-LUKS skip for disk1, got: {err!r}"
    )
    assert "skip: disk2 not LUKS-formatted" in err, (
        f"expected plain non-LUKS skip for disk2, got: {err!r}"
    )
    assert "skip: disk3 not present" in err, (
        f"expected plain absent skip for disk3, got: {err!r}"
    )
    assert "no present LUKS disks" in err, (
        f"expected no-candidates validation error, got: {err!r}"
    )
    # Ordering contract: all three skip lines precede the error.
    err_idx = err.find("no present LUKS disks")
    for marker in [
        "skip: disk1 not LUKS-formatted",
        "skip: disk2 not LUKS-formatted",
        "skip: disk3 not present",
    ]:
        idx = err.find(marker)
        assert idx < err_idx, (
            f"expected {marker!r} to precede no-candidates error; got:\n{err!r}"
        )
    # Keyfile must not have been created on the failure path.
    machine.fail("test -f /tmp/usb/braid.key")

machine.shutdown()
