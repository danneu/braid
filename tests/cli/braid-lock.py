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
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid add {key}=/dev/disk/by-id/virtio-{key} --passphrase-stdin --yes"
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

machine.shutdown()
