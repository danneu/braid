# Test: wrapper-pool-lock-not-inherited
#
# Intent:
#   While braid is mid-add (sleep inhibitor live, journal written),
#   the wrapper bash MUST be the sole holder of fd 9 on
#   /run/braid-pool.lock. The braid binary and every descendant it
#   spawns -- including the systemd-inhibit subprocess and its
#   sh + sleep child -- must NOT have fd 9 inherited.
#
# Why it exists:
#   Catches inheritance regressions in modules/braid/braid-wrapper.sh
#   directly, without relying on the SIGKILL/orphan path firing.
#   Timing-independent: the assertion holds at any point during the
#   mutation window. If `9>&-` is removed from the wrapper, this
#   test fails because /proc/<braid_pid>/fd/9 will resolve to
#   /run/braid-pool.lock.
#
# Scenario:
#   Operator runs `braid add` to grow the pool. Mid-balance, an
#   external observer (this test) inspects /proc/*/fd and confirms
#   that the only process holding the pool lock fd is the wrapper
#   bash -- the one process whose lifetime SHOULD bound the lock.

import time

start_all()
machine.wait_for_unit("multi-user.target", timeout=120)

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"

machine.succeed(
    "umask 077 && "
    f"printf '%s\\n' {passphrase} > /tmp/passphrase"
)

# --- Phase 1: Bootstrap a 1-disk pool ---
with subtest("Bootstrap 1-disk pool"):
    machine.succeed(
        "braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 disk1=/dev/disk/by-id/virtio-disk1 "
        "--passphrase-file /tmp/passphrase --yes"
    )
    machine.succeed("mountpoint -q /mnt/storage")

# --- Phase 2: Write payload so balance has real work ---
with subtest("Write urandom payload"):
    machine.succeed(
        "dd if=/dev/urandom of=/mnt/storage/payload bs=1M count=400 "
        "status=none"
    )
    machine.succeed("sync")

# --- Phase 3: Background braid add disk2; capture wrapper PID ---
#
# `nohup braid ... & echo $!` -- $! is the PID after nohup execs into
# the braid wrapper bash. No pipeline, so PID resolution is
# unambiguous (see wrapper-pool-lock-released-after-sigkill.py for
# the rationale).
with subtest("Background braid add disk2"):
    wrapper_pid = machine.succeed(
        "rm -f /tmp/add.log; "
        "nohup braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 disk2=/dev/disk/by-id/virtio-disk2 "
        "--passphrase-file /tmp/passphrase --yes "
        ">/tmp/add.log 2>&1 & echo $!"
    ).strip()
    print(f"wrapper bash pid: {wrapper_pid}")

# --- Phase 4: Wait for inhibitor + mutation window (same as sigkill test) ---
with subtest("Wait for sleep inhibitor and mutation window"):
    inh = None
    for _ in range(800):
        inh = find_braid_sleep_inhibitor(list_inhibitors())
        if inh is not None:
            break
        time.sleep(0.05)
    assert inh is not None, (
        "no braid sleep inhibitor observed.\n"
        + machine.execute("cat /tmp/add.log 2>&1")[1]
    )

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
    assert saw_mutation_window, "no mutation window observed"

# --- Phase 5: Sanity-check the wrapper IS the lock holder ---
#
# /proc/<wrapper_pid>/fd/9 must symlink to /run/braid-pool.lock.
# Without this sanity check, a buggy test that finds zero holders
# could pass for the wrong reason (e.g. lock never acquired).
with subtest("Wrapper bash holds /run/braid-pool.lock at fd 9"):
    wrapper_fd9 = machine.succeed(
        f"readlink /proc/{wrapper_pid}/fd/9"
    ).strip()
    assert wrapper_fd9 == "/run/braid-pool.lock", (
        f"wrapper fd 9 -> {wrapper_fd9!r}, expected /run/braid-pool.lock"
    )

# --- Phase 6: The actual assertion ---
#
# Find every PID with an fd symlinked to /run/braid-pool.lock. The
# only one should be the wrapper bash. If braid (or any descendant
# inheriting from it) shows up here, the `9>&-` redirect has
# regressed.
with subtest("Only the wrapper bash holds the pool lock fd"):
    # Use execute, not succeed: `find /proc/*/fd` reliably exits 1
    # (without producing stderr -- it's swallowed by 2>/dev/null) when
    # /proc entries vanish during traversal, even though the matched
    # entries are correctly emitted on stdout. We care about the
    # stdout content, not the exit code.
    _, holders_raw = machine.execute(
        "find /proc/*/fd -lname '*braid-pool.lock' 2>/dev/null"
    )
    holders_raw = holders_raw.strip()
    holder_pids = set()
    for line in holders_raw.splitlines():
        # /proc/<pid>/fd/<fdnum>
        parts = line.split("/")
        if len(parts) >= 4 and parts[1] == "proc":
            holder_pids.add(parts[2])
    assert holder_pids == {wrapper_pid}, (
        f"expected only the wrapper bash (pid {wrapper_pid}) to hold "
        f"the pool lock fd; found holders: {sorted(holder_pids)}.\n"
        f"raw find output:\n{holders_raw}\n\n"
        f"holder details:\n"
        + machine.execute(
            "for fd in $(find /proc/*/fd -lname '*braid-pool.lock' "
            "2>/dev/null); do "
            "  pid=$(echo $fd | sed 's|/proc/\\([0-9]*\\)/.*|\\1|'); "
            "  echo \"--- pid $pid ---\"; "
            "  cat /proc/$pid/cmdline 2>/dev/null | tr '\\0' ' '; "
            "  echo; "
            "done"
        )[1]
    )

# --- Phase 7: Let the add finish so cleanup is clean ---
#
# Use the same operation-done signal as add-inhibits-suspend.py:
# pending-op.json clearance is the last step of cmd_add.
with subtest("Wait for add to finish"):
    machine.wait_until_succeeds(
        "test ! -f /var/lib/braid/pending-op.json",
        timeout=600,
    )

machine.shutdown()
