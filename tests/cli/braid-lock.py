# Test: braid lock
#
# Intent: Verify `braid lock` unmounts the pool and closes all LUKS mappers
# in one idempotent command.
#
# Why it exists: `braid unlock` opens LUKS volumes and mounts the pool, but
# there is no inverse. Users must manually umount + cryptsetup close each
# mapper. `braid lock` wraps this into a single safe command.
#
# Scenario: 3-disk RAID1 pool is set up via `braid add` with test data.
# Tests exercise: happy path (mounted → locked), idempotent re-run,
# partial state (pool unmounted + one mapper pre-closed), and round-trip
# with `braid unlock` to verify data integrity.

import shlex

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"
disk_names = ["disk1", "disk2", "longdisk3"]


def add_cmd(key):
    """Build a `braid add <key> --yes` command."""
    pq = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {pq} | "
        f"braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 {key}=/dev/disk/by-id/virtio-{key} --passphrase-stdin --yes"
    )


def unlock_cmd():
    """Build a `braid unlock` command."""
    pq = shlex.quote(passphrase)
    return f"printf '%s\\n' {pq} | braid unlock --passphrase-stdin"


# --- Setup: Create a 3-disk RAID1 pool with test data ---

with subtest("Setup: create 3-disk pool"):
    for disk in disk_names:
        machine.succeed(add_cmd(disk))

    # Write test data
    machine.succeed("echo 'persistent data' > /mnt/storage/test.txt")
    machine.succeed("sync")

# Intent: Pin the live btrfs-progs exit code used to classify the benign
# finished-before-pause race in systemd-stop lock execution.
# Why it exists: Mocked Rust tests cannot detect an upstream exit-code change;
# without this lock, a nixpkgs bump could silently make shutdown fail closed.
# Scenario: The pool is mounted and idle when a late pause request arrives
# after the balance observed during planning has already finished.
with subtest("Live btrfs balance pause on an idle pool returns exit 2"):
    exit_code, stderr = machine.execute("btrfs balance pause /mnt/storage 2>&1")
    assert exit_code == 2, (
        f"expected exit 2 for an idle balance pause, got {exit_code}; "
        f"stderr: {stderr}"
    )

# --- Test 1: Happy path ---
# Intent: pool mounted, all mappers open → braid lock closes everything.
# Why: This is the primary use case — lock a running pool.
# Scenario: User wants to safely power off or detach drives.

with subtest("Test 1: happy path — mounted pool locks cleanly"):
    machine.succeed("mountpoint -q /mnt/storage")
    for k in disk_names:
        machine.succeed(f"test -e /dev/mapper/braid-{k}")

    machine.succeed("braid lock >/tmp/live-stdout 2>/tmp/live-stderr")
    live_stderr = machine.succeed("cat /tmp/live-stderr")
    live_stderr_lines = live_stderr.splitlines()
    assert "[ok]   pool: unmounted /mnt/storage" in live_stderr_lines, (
        f"expected exact live pool row, got: {live_stderr!r}"
    )
    assert "[ok]   disk longdisk3: locked" in live_stderr_lines, (
        f"expected exact long-name disk row, got: {live_stderr!r}"
    )
    # Principle 13: a [wait] row precedes every long-running subprocess.
    unmount_wait = "[wait] pool: unmounting /mnt/storage..."
    unmounted_ok = "[ok]   pool: unmounted /mnt/storage"
    assert unmount_wait in live_stderr_lines, (
        f"expected pool unmount wait row, got: {live_stderr!r}"
    )
    assert live_stderr.find(unmount_wait) < live_stderr.find(unmounted_ok), (
        f"unmount wait must precede unmount ok, got: {live_stderr!r}"
    )
    lock_wait = "[wait] disk longdisk3: locking..."
    locked_ok = "[ok]   disk longdisk3: locked"
    assert lock_wait in live_stderr_lines, (
        f"expected per-disk lock wait row, got: {live_stderr!r}"
    )
    assert live_stderr.find(lock_wait) < live_stderr.find(locked_ok), (
        f"lock wait must precede lock ok, got: {live_stderr!r}"
    )
    assert "\x1b[" not in live_stderr, (
        f"lock stderr must be plain without a TTY, got: {live_stderr!r}"
    )

    # Pool unmounted
    machine.fail("mountpoint -q /mnt/storage")

    # All mappers closed
    for k in disk_names:
        machine.fail(f"test -e /dev/mapper/braid-{k}")

# --- Test 2: Idempotent ---
# Intent: running braid lock when already locked exits 0.
# Why: Idempotency prevents scripts from failing on repeated calls.
# Scenario: Automation runs `braid lock` in a shutdown hook that may fire twice.

with subtest("Test 2: idempotent — lock again exits 0"):
    machine.succeed("braid lock")

    # Still no mappers
    machine.fail("mountpoint -q /mnt/storage")
    for k in disk_names:
        machine.fail(f"test -e /dev/mapper/braid-{k}")

# --- Test 3: Partial state ---
# Intent: pool already unmounted, one mapper already closed → braid lock
# closes the remaining mappers and reports the pre-closed one.
# Why: After a crash or manual intervention, state may be inconsistent.
# Scenario: User manually umounted and closed one disk, then runs braid lock.

with subtest("Test 3: partial state — closes remaining mappers"):
    # Bring pool back up first
    machine.succeed(unlock_cmd())
    machine.succeed("mountpoint -q /mnt/storage")

    # Manually unmount and close disk1
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup close braid-disk1")

    # disk2 and longdisk3 still open
    machine.succeed("test -e /dev/mapper/braid-disk2")
    machine.succeed("test -e /dev/mapper/braid-longdisk3")

    machine.succeed("braid lock")

    # All mappers now closed
    for k in disk_names:
        machine.fail(f"test -e /dev/mapper/braid-{k}")

# --- Test 4: Round-trip ---
# Intent: braid lock then braid unlock restores the pool with data intact.
# Why: Lock must not corrupt state; unlock must recover cleanly after lock.
# Scenario: User locks pool, then unlocks later — data must survive.

with subtest("Test 4: round-trip — lock then unlock, data intact"):
    machine.succeed(unlock_cmd())

    machine.succeed("mountpoint -q /mnt/storage")
    for k in disk_names:
        machine.succeed(f"test -e /dev/mapper/braid-{k}")

    content = machine.succeed("cat /mnt/storage/test.txt").strip()
    assert content == "persistent data", f"Expected 'persistent data', got '{content}'"

# --- Test 5: Dry-run preview stream routing ---
# Intent: `braid lock --dry-run` writes its entire preview to stdout with
# nothing on stderr when /dev/mapper is readable and there is nothing to do.
# Why it exists: the dry-run preview is the single-stream contract the
# render_lock_dry_run helper establishes; if the CLI reverts to mixing
# streams (e.g. swapping print! for eprintln!), users piping
# `braid lock --dry-run > preview.txt` would silently lose output.
# Scenario: operator scripts a shutdown rehearsal by redirecting the
# preview to a file; capturing stdout alone must contain the full preview.

with subtest("Test 5: dry-run preview goes to stdout"):
    # Lock the pool so dry-run has no work to do -- shortest deterministic preview.
    machine.succeed("braid lock")
    machine.fail("mountpoint -q /mnt/storage")
    for k in disk_names:
        machine.fail(f"test -e /dev/mapper/braid-{k}")

    machine.succeed(
        "braid lock --dry-run >/tmp/lock-stdout 2>/tmp/lock-stderr"
    )
    stdout = machine.succeed("cat /tmp/lock-stdout")
    stderr = machine.succeed("cat /tmp/lock-stderr")

    assert stdout == "nothing to do.\n", f"unexpected stdout: {stdout!r}"
    assert "\x1b[" not in stdout, f"dry-run stdout must be plain without a TTY: {stdout!r}"
    assert stderr == "", f"expected empty stderr, got: {stderr!r}"

# --- Test 6: Dry-run unverified mapper warning stream routing ---
# Intent: `braid lock --dry-run` routes unverified cleanup-candidate warnings
# to stdout and keeps stderr empty.
# Why it exists: successful dry-run owns a single rendered Preview stream; a
# skipped `braid-*` candidate must not leak a planner warning to stderr or
# render a false clean no-op.
# Scenario: the pool is already locked, but a stale `braid-disk1` path exists
# in `/dev/mapper` without a verifiable cryptsetup backing UUID.

with subtest("Test 6: dry-run unverified mapper warning goes to stdout"):
    machine.succeed("touch /dev/mapper/braid-disk1")
    machine.succeed(
        "braid lock --dry-run >/tmp/lock-skip-stdout 2>/tmp/lock-skip-stderr"
    )
    skip_stdout = machine.succeed("cat /tmp/lock-skip-stdout")
    skip_stderr = machine.succeed("cat /tmp/lock-skip-stderr")
    machine.succeed("rm -f /dev/mapper/braid-disk1")

    assert "[warn] skipping mapper braid-disk1: cannot verify backing LUKS UUID" in skip_stdout, (
        f"expected skip warning on stdout, got: {skip_stdout!r}"
    )
    assert "cleanup incomplete: some braid mappers could not be verified" in skip_stdout, (
        f"expected cleanup-incomplete info on stdout, got: {skip_stdout!r}"
    )
    assert "nothing to do." not in skip_stdout, (
        f"uncertain cleanup must not render a clean no-op, got: {skip_stdout!r}"
    )
    assert "close LUKS mapper braid-disk1" not in skip_stdout, (
        f"skipped mapper must not appear as a close step, got: {skip_stdout!r}"
    )
    assert skip_stderr == "", f"dry-run stderr must be empty, got: {skip_stderr!r}"

# --- Test 7: Real-run unverified mapper is not a clean no-op ---
# Intent: `braid lock` suppresses `pool already locked` when cleanup is
# uncertain because a `braid-*` candidate could not be verified.
# Why it exists: warning-only real execution must not claim a clean locked
# state while leaving a braid-prefixed candidate open.
# Scenario: same stale `/dev/mapper/braid-disk1` path as the dry-run stream
# test, but through real execution.

with subtest("Test 7: real-run unverified mapper suppresses already-locked"):
    machine.succeed("touch /dev/mapper/braid-disk1")
    machine.succeed(
        "braid lock >/tmp/lock-skip-real-stdout 2>/tmp/lock-skip-real-stderr"
    )
    real_stdout = machine.succeed("cat /tmp/lock-skip-real-stdout")
    real_stderr = machine.succeed("cat /tmp/lock-skip-real-stderr")
    machine.succeed("rm -f /dev/mapper/braid-disk1")

    assert real_stdout == "", f"real lock should not write stdout, got: {real_stdout!r}"
    assert "[warn] skipping mapper braid-disk1: cannot verify backing LUKS UUID" in real_stderr, (
        f"expected skip warning on stderr, got: {real_stderr!r}"
    )
    assert "disk disk1: already closed" not in real_stderr, (
        f"skipped mapper must suppress matching already-closed row, got: {real_stderr!r}"
    )
    assert "pool already locked" not in real_stderr, (
        f"uncertain cleanup must not print already-locked, got: {real_stderr!r}"
    )

machine.shutdown()
