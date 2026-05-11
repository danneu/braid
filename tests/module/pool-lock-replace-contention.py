# Test: pool-lock-replace-contention
#
# Intent: When another process holds /run/braid-pool.lock, `braid
# replace` must fail fast before it can write pending-op.json.
#
# Why it exists: `replace` has a race window from preflight
# `check_no_pending_operation` (cli/src/replace.rs:877) to journal write
# (cli/src/replace.rs:476), and state_io::atomic_write uses the
# deterministic .pending-op.json.tmp path (cli/src/state_io.rs:62). A
# concurrent replace that reaches that write can clobber another
# operation's journal before btrfs rejects the second kernel replace.
#
# Scenario: Admin starts one pool mutation in one shell, then runs
# `braid replace --old disk2 --new disk4=...` in another shell against a
# mounted 3-disk pool with a spare disk. The second command should fail
# at the wrapper lock and leave no pending-op.json behind.

import shlex


start_all()
machine.wait_for_unit("multi-user.target", timeout=120)

passphrase = "testpassphrase"
pq = shlex.quote(passphrase)


def add_cmd(key):
    return (
        f"printf '%s\\n' {pq} | "
        "braid add "
        "--luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 "
        "--luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 "
        f"{key}=/dev/disk/by-id/virtio-{key} --passphrase-stdin --yes"
    )


with subtest("Build 3-disk pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed(add_cmd("disk3"))
    machine.succeed("mountpoint -q /mnt/storage")

with subtest("braid replace fails fast when pool lock is held"):
    holder_pid = machine.succeed(
        "rm -f /tmp/holder.ready; "
        "nohup flock -x /run/braid-pool.lock "
        "sh -c 'touch /tmp/holder.ready; sleep 60' "
        ">/dev/null 2>&1 & echo $!"
    ).strip()
    machine.wait_until_succeeds("test -e /tmp/holder.ready", timeout=10)
    locks = machine.succeed("cat /proc/locks")
    assert "FLOCK" in locks, "no flock in /proc/locks: " + locks

    rc, out = machine.execute(
        f"timeout 5 sh -c \"printf '%s\\n' {pq} | "
        "braid replace "
        "--luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 "
        "--luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 "
        "--old disk2 --new disk4=/dev/disk/by-id/virtio-disk4 "
        "--passphrase-stdin --yes\" 2>&1"
    )
    machine.execute(f"kill {holder_pid} 2>/dev/null || true")

    assert rc != 0, "expected rc != 0; out=" + out
    assert rc != 124, "replace hung past 5s cap; out=" + out
    assert "another braid operation is already in progress" in out, (
        "expected contention message; out=" + out
    )
    machine.fail("test -e /var/lib/braid/pending-op.json")
