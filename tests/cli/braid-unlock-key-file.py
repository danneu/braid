# Test: braid-unlock-key-file
#
# Intent: Verify `--key-file` flag opens LUKS with a binary keyfile and
# that a wrong keyfile is rejected.
#
# Why it exists: The keyfile unlock code path is entirely different from
# passphrase (no PBKDF, different cryptsetup flags, run() vs run_with_stdin).
# Must verify independently that correct keyfile succeeds, wrong keyfile
# fails, and passphrase enrollment is not corrupted by keyfile enrollment.
#
# Scenario: Pool set up with passphrase (slot 0) and keyfile (slot 1).
# Lock. Unlock with correct keyfile. Lock. Try wrong keyfile (fail).
# Unlock with passphrase (still works).

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


# --- Setup ---

with subtest("Setup: create pool and enroll keyfile"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed("echo 'keyfile unlock test' > /mnt/storage/test.txt")
    machine.succeed("sync")

    # Generate and enroll keyfile
    machine.succeed("dd if=/dev/urandom of=/tmp/braid.key bs=4096 count=1 iflag=fullblock")
    machine.succeed("chmod 400 /tmp/braid.key")
    pq = shlex.quote(passphrase)
    machine.succeed(
        f"printf '%s\\n' {pq} | braid enroll /tmp --passphrase-stdin"
    )

    # Generate a wrong keyfile
    machine.succeed("dd if=/dev/urandom of=/tmp/wrong.key bs=4096 count=1 iflag=fullblock")

# --- Test 1: Correct keyfile unlocks pool ---

with subtest("Test 1: correct keyfile unlocks"):
    close_all()
    machine.succeed(
        "braid unlock --key-file /tmp/braid.key >/tmp/kfu.out 2>/tmp/kfu.err"
    )
    err = machine.succeed("cat /tmp/kfu.err")
    wait_line = "[wait] keyfile: checking against disk1...\n"
    accepted_line = "[ok]   keyfile: accepted by disk1\n"
    unlocked_line = "[ok]   disk disk1: unlocked\n"
    assert wait_line in err, (
        f"expected keyfile verification wait line, got: {err!r}"
    )
    assert accepted_line in err, (
        f"expected keyfile accepted row, got: {err!r}"
    )
    assert err.find(wait_line) < err.find(accepted_line), (
        f"keyfile wait must precede accepted row, got: {err!r}"
    )
    assert err.find(accepted_line) < err.find(unlocked_line), (
        f"keyfile accepted row must precede first unlocked row, got: {err!r}"
    )
    unlocking_wait = "[wait] disk disk1: unlocking...\n"
    mounting_wait = "[wait] pool: mounting /mnt/storage...\n"
    mounted_line = "[ok]   pool: mounted /mnt/storage\n"
    assert unlocking_wait in err, (
        f"per-disk unlocking wait row missing, got: {err!r}"
    )
    assert err.find(unlocking_wait) < err.find(unlocked_line), (
        f"unlocking wait must precede unlocked row, got: {err!r}"
    )
    assert mounting_wait in err, (
        f"pool mounting wait row missing, got: {err!r}"
    )
    assert err.find(mounting_wait) < err.find(mounted_line), (
        f"mounting wait must precede mounted row, got: {err!r}"
    )
    machine.succeed("mountpoint -q /mnt/storage")
    content = machine.succeed("cat /mnt/storage/test.txt").strip()
    assert content == "keyfile unlock test", f"Expected 'keyfile unlock test', got '{content}'"

# --- Test 2: Wrong keyfile is rejected ---

with subtest("Test 2: wrong keyfile rejected"):
    close_all()
    ret = machine.execute("braid unlock --key-file /tmp/wrong.key 2>&1")
    assert ret[0] == 1, f"expected exit 1 for wrong keyfile, got {ret[0]}"
    assert "wrong keyfile (rejected by disk1)" in ret[1], (
        f"expected wrong-keyfile wording, got: {ret[1]!r}"
    )
    machine.fail("mountpoint -q /mnt/storage")

# Intent: braid unlock --key-file rejects a wrong-size keyfile at the CLI
#   validation boundary, before any cryptsetup keyfile verify/open invocation.
# Why it exists: prior to the shared validator, unlock surfaced cryptsetup's
#   generic short-read failure instead of a clear braid-level size error.
# Scenario: an admin points --key-file at an undersized placeholder and must
#   see a message that names the 4096-byte contract.
with subtest("Test 2c: wrong-size keyfile rejected with clear error"):
    close_all()
    machine.succeed("printf 'short' > /tmp/wrong-size.key")
    ret = machine.execute("braid unlock --key-file /tmp/wrong-size.key 2>&1")
    assert ret[0] == 1, f"expected exit 1 for wrong-size keyfile, got {ret[0]}"
    assert "4096" in ret[1], (
        f"error must name 4096-byte contract, got: {ret[1]!r}"
    )
    machine.fail("mountpoint -q /mnt/storage")

# --- Test 2b: keyfile + missing disk -- exit 2 + --allow-degraded hint ---
#
# Intent: --key-file with a missing pool member and no --allow-degraded must
#   exit 2 and print the --allow-degraded hint. This is the exact contract
#   braid-auto-unlock.service consumes at storage.nix:265.
# Why it exists: Existing DegradedRefused tests all run via passphrase
#   (braid-unlock.py Tests 4a/4a_dry/7). A future refactor that routes
#   keyfile DegradedRefused through a different exit code would break the
#   auto-unlock unit's hint route while every other assertion still passed.
#   This subtest pins the keyfile branch directly.
# Scenario: 2-disk pool unlocked with keyfile so far. Close the pool, delete
#   disk2's by-id symlink so plan_open_pool classifies disk2 as Absent.
#   Re-run with --key-file (no --allow-degraded). Expect exit 2,
#   "--allow-degraded" on stderr, no mount. Restore symlink before Test 3.
with subtest("Test 2b: --key-file with missing disk -- exit 2 + --allow-degraded hint"):
    close_all()
    machine.succeed("rm -f /dev/disk/by-id/virtio-disk2")

    ret = machine.execute("braid unlock --key-file /tmp/braid.key 2>&1")
    assert ret[0] == 2, f"expected exit 2 for keyfile degraded refusal, got {ret[0]}"
    assert "--allow-degraded" in ret[1], (
        f"expected --allow-degraded hint on stderr, got: {ret[1]!r}"
    )
    machine.fail("mountpoint -q /mnt/storage")

    # Restore symlink for Test 3 (the virtio symlinks are managed by udev).
    machine.succeed("udevadm trigger && udevadm settle")
    machine.succeed("test -e /dev/disk/by-id/virtio-disk2")

# --- Test 3: Passphrase still works ---

with subtest("Test 3: passphrase still works"):
    close_all()
    pq = shlex.quote(passphrase)
    machine.succeed(f"printf '%s\\n' {pq} | braid unlock --passphrase-stdin")
    machine.succeed("mountpoint -q /mnt/storage")
    content = machine.succeed("cat /mnt/storage/test.txt").strip()
    assert content == "keyfile unlock test", f"Expected 'keyfile unlock test', got '{content}'"

machine.shutdown()
