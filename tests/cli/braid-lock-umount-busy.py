# Test: braid lock — umount busy gives actionable error
#
# Intent: When umount fails because a process holds a file open, `braid lock`
# fails with an error message that includes a hint about `lsof` or `fuser`
# so the user knows how to find the blocker.
#
# Why it exists: The raw umount stderr ("target is busy") gives no actionable
# information. Users need to know to run `lsof` or `fuser` to identify the
# process holding the mount busy. Without this hint, they're stuck.
#
# Scenario: 2-disk pool via `braid add`. A background `tail -f` holds a file
# open. `braid lock` fails with an actionable hint. After killing the blocker,
# `braid lock` succeeds and all mappers are closed.

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
        f"braid add {key} --passphrase-stdin --yes"
    )


# --- Setup ---

with subtest("Setup: create 2-disk pool with test file"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed("echo 'test data' > /mnt/storage/test.txt")
    machine.succeed("sync")

# --- Test 1: braid lock fails with actionable hint when mount is busy ---

with subtest("braid lock fails with lsof/fuser hint when mount is busy"):
    # Hold the mount busy with tail -f in the background
    machine.succeed("nohup tail -f /mnt/storage/test.txt > /dev/null 2>&1 &")

    exit_code, output = machine.execute("braid lock 2>&1")
    print(f"Exit code: {exit_code}")
    print(f"Output: {output}")
    assert exit_code != 0, f"Expected braid lock to fail, but exit was {exit_code}"
    output_lower = output.lower()
    assert "lsof" in output_lower or "fuser" in output_lower, \
        f"Expected hint about 'lsof' or 'fuser' in error output, got:\n{output}"

# --- Test 2: After killing blocker, braid lock succeeds ---

with subtest("After killing blocker, braid lock succeeds"):
    machine.succeed("pkill -f 'tail -f /mnt/storage/test.txt'")
    machine.succeed("braid lock")

    machine.fail("mountpoint -q /mnt/storage")
    for k in ["disk1", "disk2"]:
        machine.fail(f"test -e /dev/mapper/braid-{k}")

machine.shutdown()
