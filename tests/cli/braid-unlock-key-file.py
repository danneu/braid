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
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid add {key}=/dev/disk/by-id/virtio-{key} --passphrase-stdin --yes"
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
    unlocked_line = "[ok]   disk disk1: unlocked\n"
    assert wait_line in err, (
        f"expected keyfile verification wait line, got: {err!r}"
    )
    assert err.find(wait_line) < err.find(unlocked_line), (
        f"wait line must precede first unlocked row, got: {err!r}"
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
    assert ret[0] != 0, "Expected non-zero exit for wrong keyfile"
    machine.fail("mountpoint -q /mnt/storage")

# --- Test 3: Passphrase still works ---

with subtest("Test 3: passphrase still works"):
    close_all()
    pq = shlex.quote(passphrase)
    machine.succeed(f"printf '%s\\n' {pq} | braid unlock --passphrase-stdin")
    machine.succeed("mountpoint -q /mnt/storage")
    content = machine.succeed("cat /mnt/storage/test.txt").strip()
    assert content == "keyfile unlock test", f"Expected 'keyfile unlock test', got '{content}'"

machine.shutdown()
