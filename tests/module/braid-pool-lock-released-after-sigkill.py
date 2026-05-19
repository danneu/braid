# Test: braid-pool-lock-released-after-sigkill
#
# Intent:
#   When the braid binary is SIGKILL'd mid-add, /run/braid-pool.lock must be
#   released within seconds and a fresh non-blocking flock must succeed.
#
# Why it exists:
#   Rust owns the flock on an O_CLOEXEC fd. The process-death path must not be
#   pinned by any inherited child fd, otherwise recovery commands see false
#   "another braid operation" contention after the mutator is gone.
#
# Scenario:
#   Operator starts `braid add`, the braid process dies during the long balance
#   window, and the next recovery action must be able to acquire the lock.

import time

start_all()
machine.wait_for_unit("multi-user.target", timeout=120)

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

with subtest("SIGKILL the braid binary"):
    machine.succeed(f"kill -9 {braid_pid}")
    machine.wait_until_fails(f"kill -0 {braid_pid} 2>/dev/null", timeout=10)

with subtest("No fd holds /run/braid-pool.lock after process death"):
    deadline = time.monotonic() + 5
    holders = ""
    while time.monotonic() < deadline:
        _, out = machine.execute("find /proc/*/fd -lname '*braid-pool.lock' 2>/dev/null")
        holders = out.strip()
        if holders == "":
            break
        time.sleep(0.1)
    assert holders == "", "braid-pool.lock fd still held after SIGKILL:\n" + holders

with subtest("flock -n succeeds"):
    machine.succeed("flock -n /run/braid-pool.lock true")

with subtest("braid recover proceeds"):
    rc, out = machine.execute("braid recover --passphrase-file /tmp/passphrase 2>&1")
    assert rc == 0, f"braid recover failed: rc={rc}\n{out}"

machine.shutdown()
