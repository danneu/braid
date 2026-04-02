# Intent: braid add must wait for an in-flight balance and succeed via --enqueue.
# Why: validates the sysfs-based exclusive op preflight + --enqueue wiring
#   end-to-end against a real kernel, not just unit test mocks.
# Scenario: operator has a 2-disk RAID1 pool. A background balance is running
#   (RAID1 → single conversion to create observable work). Operator runs
#   `braid add disk3`. Braid detects the active balance via sysfs, prints
#   a "waiting" message, and --enqueue blocks until the balance finishes.
#   The add then succeeds and disk3 appears in the pool.

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


# 1. Create 2-disk RAID1 pool
with subtest("create 2-disk pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed("mountpoint -q /mnt/storage")

# 2. Write data so balance has observable work
with subtest("write test data"):
    machine.succeed("dd if=/dev/urandom of=/mnt/storage/bigfile bs=1M count=512")
    machine.succeed("sync")

# 3. Start a background balance, synchronize on observed running state,
#    then run braid add and verify it waits and succeeds.
with subtest("braid add waits for balance and succeeds"):
    # Start balance in background (RAID1 → single conversion gives real work)
    machine.execute(
        "btrfs balance start -dconvert=single -mconvert=dup -f /mnt/storage "
        "> /tmp/balance.log 2>&1 &"
    )

    # Synchronize: poll until balance is confirmed running.
    # Drive decisions from observed kernel state, not timing assumptions.
    # The test MUST observe an active balance — otherwise it degrades to
    # testing the no-contention path, which existing tests already cover.
    saw_running = False
    for i in range(200):
        ret = machine.execute("btrfs balance status /mnt/storage")
        if "running" in ret[1].lower():
            saw_running = True
            break
        import time
        time.sleep(0.05)

    assert saw_running, (
        "Never observed balance in 'running' state — test cannot exercise "
        "the sysfs preflight + --enqueue wait path"
    )

    # Run braid add disk3. The balance is running, so braid will detect it
    # via sysfs, print a wait message, and --enqueue will block until the
    # balance finishes.
    result = machine.execute(add_cmd("disk3") + " 2>&1")
    exit_code = result[0]
    output = result[1]

    assert exit_code == 0, f"braid add disk3 failed (exit {exit_code}):\n{output}"

    # The wait message proves braid saw the active op via sysfs and proceeded
    # with --enqueue rather than erroring or skipping the check.
    assert "waiting for in-flight" in output.lower(), (
        f"expected 'waiting for in-flight' message in output:\n{output}"
    )

    # Verify disk3 is in the pool
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "/dev/mapper/braid-disk3" in fi_show, (
        f"braid-disk3 missing from pool:\n{fi_show}"
    )

machine.shutdown()
