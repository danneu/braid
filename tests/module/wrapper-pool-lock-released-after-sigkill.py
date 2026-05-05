# Test: wrapper-pool-lock-released-after-sigkill
#
# Intent:
#   When the braid binary is SIGKILL'd mid-`braid add`, the wrapper
#   bash exits cleanly and releases its fd 9 on
#   /run/braid-pool.lock. With the `9>&-` redirect on the wrapper's
#   braid invocation, the only holder of fd 9 was the wrapper bash;
#   so flock is released within seconds of the SIGKILL and the next
#   `flock -n /run/braid-pool.lock true` succeeds.
#
# Why it exists:
#   This protects against re-introducing the fd-inheritance bug in
#   modules/braid/braid-wrapper.sh. Without `9>&-`, the long-lived
#   systemd-inhibit subprocess from cli/src/inhibit.rs (in its own
#   pgroup, surviving SIGKILL of braid) inherits fd 9 and keeps the
#   advisory flock held until manually killed. The test must FAIL if
#   `9>&-` is removed.
#
# Scenario:
#   Operator starts a `braid add` to grow the pool with a new disk.
#   While the post-add pool_balance_raid1 is in flight (the long
#   mutation window where Ctrl-C / SIGKILL / OOM are realistic),
#   the braid binary dies without running Drop. The wrapper detects
#   the exit, releases its fd 9, and the next braid invocation (e.g.
#   `braid recover`) must be able to acquire the lock -- otherwise
#   the operator hits "another braid operation is already in
#   progress" with no actual operation running.

import time

start_all()
machine.wait_for_unit("multi-user.target", timeout=120)

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"

# Write the passphrase to a file so we can use --passphrase-file. This
# avoids the `printf | braid add --passphrase-stdin` pipeline shape:
# under `cmd1 | cmd2 &`, $! is cmd2's PID and the printf side adds
# noise to PID resolution. With --passphrase-file the backgrounded
# command is a single process and $! unambiguously points at the
# braid wrapper bash.
machine.succeed(
    "umask 077 && "
    f"printf '%s\\n' {passphrase} > /tmp/passphrase"
)

# --- Phase 1: Bootstrap a 1-disk pool ---
#
# Use real `braid add` (not initrd-fixture) so the wrapper's flock
# acquisition is exercised end-to-end during setup. This also matches
# the actual mutation window the regression covers.
with subtest("Bootstrap 1-disk pool"):
    machine.succeed(
        "braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 disk1=/dev/disk/by-id/virtio-disk1 "
        "--passphrase-file /tmp/passphrase --yes"
    )
    machine.succeed("mountpoint -q /mnt/storage")

# --- Phase 2: Write a single-profile payload ---
#
# Without this, pool_balance_raid1 has nothing to do and the
# mutation/inhibitor window collapses before we can land the SIGKILL
# inside it. 400 MiB matches add-inhibits-suspend.py's payload.
with subtest("Write urandom payload (single-profile chunks)"):
    machine.succeed(
        "dd if=/dev/urandom of=/mnt/storage/payload bs=1M count=400 "
        "status=none"
    )
    machine.succeed("sync")

# --- Phase 3: Start `braid add disk2` in background; capture wrapper PID ---

with subtest("Background braid add disk2 and capture wrapper PID"):
    wrapper_pid = machine.succeed(
        "rm -f /tmp/add.log; "
        "nohup braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 disk2=/dev/disk/by-id/virtio-disk2 "
        "--passphrase-file /tmp/passphrase --yes "
        ">/tmp/add.log 2>&1 & echo $!"
    ).strip()
    print(f"wrapper bash pid: {wrapper_pid}")

# --- Phase 4: Wait for the bug-trigger window to actually open ---
#
# Two independent readiness signals must fire before we SIGKILL:
#   1. The sleep inhibitor is registered (the systemd-inhibit
#      subprocess exists -- this is the descendant that inherits fd
#      9 in the buggy build).
#   2. EITHER /var/lib/braid/pending-op.json exists (journal written
#      -- braid is past the irreversible boundary) OR
#      /sys/fs/btrfs/*/exclusive_operation reports balance.
#
# The double signal matters because cmd_add acquires the inhibitor
# BEFORE journal::write_journal (cli/src/add.rs:492 vs
# cli/src/add.rs:520). An inhibitor-only signal can race the test
# into killing braid before pending-op.json is written, after which a
# subsequent `braid recover` could fail for journal reasons (not
# fd-inheritance reasons) and obscure the actual assertion.

with subtest("Wait for sleep inhibitor and mutation window"):
    inh = None
    for _ in range(800):
        inh = find_braid_sleep_inhibitor(list_inhibitors())
        if inh is not None:
            break
        time.sleep(0.05)
    assert inh is not None, (
        "no braid sleep inhibitor observed -- braid add either failed "
        "before reaching the inhibitor seam or completed too quickly. "
        "/tmp/add.log:\n" + machine.execute("cat /tmp/add.log 2>&1")[1]
    )
    print(f"inhibitor pid: {inh['pid']}")

    saw_mutation_window = False
    for _ in range(800):
        if machine.execute("test -f /var/lib/braid/pending-op.json")[0] == 0:
            saw_mutation_window = True
            break
        rc, out = machine.execute(
            "cat /sys/fs/btrfs/*/exclusive_operation 2>/dev/null"
        )
        if rc == 0 and "balance" in out.strip().lower():
            saw_mutation_window = True
            break
        time.sleep(0.05)
    assert saw_mutation_window, (
        "neither /var/lib/braid/pending-op.json nor btrfs balance "
        "appeared during the wait window. /tmp/add.log:\n"
        + machine.execute("cat /tmp/add.log 2>&1")[1]
    )

# --- Phase 5: SIGKILL the braid binary ---
#
# Kill the binary, NOT the wrapper. The wrapper bash IS the lock
# holder; killing it directly would release fd 9 cleanly even with
# the bug. The bug surface is: braid dies, the wrapper releases its
# fd 9 cleanly, but the orphaned systemd-inhibit (which inherited fd
# 9) keeps the OFD alive.

with subtest("SIGKILL the braid binary (child of wrapper)"):
    braid_pid = machine.succeed(f"pgrep -P {wrapper_pid} braid").strip()
    print(f"braid binary pid: {braid_pid}")
    machine.succeed(f"kill -9 {braid_pid}")
    # Wait for the wrapper bash to detect braid's exit and exit itself
    # (releasing its own fd 9). The wrapper exit is what brings the
    # holder count down to {systemd-inhibit only} in the buggy build.
    machine.wait_until_fails(f"kill -0 {wrapper_pid} 2>/dev/null", timeout=10)

# --- Phase 6: Assert lock is released ---
#
# In the buggy build, the orphaned systemd-inhibit shows up here and
# this assertion fails. With the `9>&-` fix, the wrapper bash was the
# only fd-9 holder; once it exits, no fd remains.

with subtest("No fd holds /run/braid-pool.lock after wrapper exit"):
    deadline = time.monotonic() + 5
    holders = None
    while time.monotonic() < deadline:
        rc, out = machine.execute(
            "find /proc/*/fd -lname '*braid-pool.lock' 2>/dev/null"
        )
        holders = out.strip()
        if holders == "":
            break
        time.sleep(0.1)
    if holders != "":
        # Surface the orphan(s) in the failure message so a future
        # debugger sees the orphaned systemd-inhibit immediately.
        diag = machine.execute(
            "for fd in $(find /proc/*/fd -lname '*braid-pool.lock' "
            "2>/dev/null); do "
            "  pid=$(echo $fd | sed 's|/proc/\\([0-9]*\\)/.*|\\1|'); "
            "  echo \"=== pid $pid ===\"; "
            "  cat /proc/$pid/cmdline 2>/dev/null | tr '\\0' ' '; "
            "  echo; "
            "  ps -o pid,ppid,pgid,stat,etime,args -p $pid 2>/dev/null; "
            "done"
        )[1]
        raise AssertionError(
            "braid-pool.lock fd still held 5s after wrapper exit -- "
            "the fd-9 inheritance bug has regressed.\n"
            f"holders:\n{holders}\n\ndetails:\n{diag}"
        )

with subtest("flock -n succeeds (lock is acquirable)"):
    # Direct kernel-level acquirability check. Avoid `braid recover`
    # here because recover can fail for unrelated reasons (no journal,
    # recovery logic refusing the half-written state) and would
    # obscure whether the lock specifically was the problem.
    machine.succeed("flock -n /run/braid-pool.lock true")

# --- Phase 7: End-to-end smoke (separate subtest) ---
#
# Now that we've proven the lock is acquirable, verify braid as a
# whole can actually proceed. Kept segmented from the lock assertion
# so a recover-side failure cannot masquerade as a fd-leak failure.

with subtest("braid recover proceeds (end-to-end smoke)"):
    rc, out = machine.execute(
        "braid recover --passphrase-file /tmp/passphrase 2>&1"
    )
    assert rc == 0, f"braid recover failed: rc={rc}\n{out}"

machine.shutdown()
