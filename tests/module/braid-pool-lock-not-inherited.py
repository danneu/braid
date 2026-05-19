# Test: braid-pool-lock-not-inherited
#
# Intent:
#   While braid is mid-add, the braid binary must be the only process with
#   an fd open on /run/braid-pool.lock. Descendants such as systemd-inhibit,
#   sh, and sleep must not inherit the lock fd.
#
# Why it exists:
#   Rust now owns the pool lock via O_CLOEXEC. If the fd is inherited by a
#   long-lived child, the kernel flock can remain held after braid exits and
#   every later mutator reports false contention.
#
# Scenario:
#   Operator grows the pool with `braid add`. Mid-balance, an external
#   observer inspects /proc/*/fd and confirms the lock lifetime is bounded by
#   the braid binary itself, not by any child process.

import time

start_all()
machine.wait_for_unit("multi-user.target", timeout=120)

passphrase = "testpassphrase"

machine.succeed("umask 077 && printf '%s\\n' testpassphrase > /tmp/passphrase")

with subtest("Bootstrap 1-disk pool"):
    machine.succeed(
        "braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 "
        "--luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 "
        "disk1=/dev/disk/by-id/virtio-disk1 --passphrase-file /tmp/passphrase --yes"
    )
    machine.succeed("mountpoint -q /mnt/storage")

with subtest("Write urandom payload"):
    machine.succeed("dd if=/dev/urandom of=/mnt/storage/payload bs=1M count=400 status=none")
    machine.succeed("sync")

with subtest("Background braid add disk2"):
    braid_pid = machine.succeed(
        "rm -f /tmp/add.log; "
        "nohup braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 "
        "--luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 "
        "disk2=/dev/disk/by-id/virtio-disk2 --passphrase-file /tmp/passphrase --yes "
        ">/tmp/add.log 2>&1 & echo $!"
    ).strip()
    print(f"braid pid: {braid_pid}")

with subtest("Wait for sleep inhibitor and mutation window"):
    inh = None
    for _ in range(800):
        inh = find_braid_sleep_inhibitor(list_inhibitors())
        if inh is not None:
            break
        time.sleep(0.05)
    assert inh is not None, "no braid sleep inhibitor observed:\n" + machine.execute("cat /tmp/add.log 2>&1")[1]

    saw_mutation_window = False
    for _ in range(800):
        if machine.execute("test -f /var/lib/braid/pending-op.json")[0] == 0:
            saw_mutation_window = True
            break
        rc, out = machine.execute("cat /sys/fs/btrfs/*/exclusive_operation 2>/dev/null")
        if rc == 0 and "balance" in out.strip().lower():
            saw_mutation_window = True
            break
        time.sleep(0.05)
    assert saw_mutation_window, "no mutation window observed"

with subtest("Only the braid binary holds the pool lock fd"):
    _, holders_raw = machine.execute("find /proc/*/fd -lname '*braid-pool.lock' 2>/dev/null")
    holders_raw = holders_raw.strip()
    holder_pids = set()
    for line in holders_raw.splitlines():
        parts = line.split("/")
        if len(parts) >= 4 and parts[1] == "proc":
            holder_pids.add(parts[2])
    assert holder_pids == {braid_pid}, (
        f"expected only braid pid {braid_pid} to hold the pool lock fd; "
        f"found {sorted(holder_pids)}\nraw find output:\n{holders_raw}"
    )

with subtest("Wait for add to finish"):
    machine.wait_until_succeeds("test ! -f /var/lib/braid/pending-op.json", timeout=600)

machine.shutdown()
