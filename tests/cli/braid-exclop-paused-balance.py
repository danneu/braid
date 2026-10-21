# Intent: braid add fails fast when a balance is paused, and braid lock
#   proceeds under it and leaves it paused for the next unlock to report.
# Why: a paused balance holds the exclusive lock indefinitely -- --enqueue
#   would hang forever, so add must detect it via sysfs and error immediately.
#   Lock used to refuse it too, telling the operator to wait for a balance
#   that never finishes on its own. Unmount keeps a paused balance paused and
#   braid mounts with skip_balance, so lock now unmounts and warns instead.
# Scenario: operator has a 2-disk pool with a paused balance. They try to add
#   a third disk (should fail with "paused" message), then lock the pool
#   (should succeed with a paused-balance warning), then unlock it (the
#   balance is still paused and unlock warns about it).

import shlex

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def add_cmd(key):
    pq = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {pq} | "
        f"braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 {key}=/dev/disk/by-id/virtio-{key} --passphrase-stdin --yes"
    )


# 1. Create 2-disk RAID1 pool
with subtest("create 2-disk pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed("mountpoint -q /mnt/storage")

# 2. Write data so balance has real work
with subtest("write test data"):
    machine.succeed("dd if=/dev/urandom of=/mnt/storage/bigfile bs=1M count=512")
    machine.succeed("sync")

# 3. Start and pause a balance via the shared
#    balance_helpers.pause_balance_with_remaining_work helper.
with subtest("start and pause balance"):
    pause_balance_with_remaining_work(machine)

# 4. With balance reliably paused, test that braid add fails fast.
with subtest("braid add fails fast on paused balance"):
    result = machine.execute(add_cmd("disk3") + " 2>&1")
    exit_code = result[0]
    output = result[1]

    assert exit_code != 0, (
        f"braid add should have failed with paused balance, but exited 0:\n{output}"
    )
    assert "paused" in output.lower(), (
        f"expected 'paused' in error output:\n{output}"
    )

# 5. braid lock proceeds under the paused balance and warns that it stays paused.
with subtest("braid lock proceeds on paused balance"):
    exit_code, output = machine.execute("braid lock 2>&1")

    assert exit_code == 0, (
        f"braid lock should succeed under a paused balance, but exited {exit_code}:\n{output}"
    )
    assert "[warn] btrfs balance is paused -- lock leaves it paused" in output, (
        f"expected paused-balance warning in lock output:\n{output}"
    )
    assert "Wait for it to finish" not in output, (
        f"lock must not tell the operator to wait for a paused balance:\n{output}"
    )

    machine.fail("mountpoint -q /mnt/storage")
    machine.fail("test -e /dev/mapper/braid-disk1")
    machine.fail("test -e /dev/mapper/braid-disk2")

# 6. The balance survives lock: after unlock it is still paused, and unlock warns.
with subtest("unlock finds the balance still paused and warns"):
    pq = shlex.quote(passphrase)
    exit_code, output = machine.execute(
        f"printf '%s\\n' {pq} | braid unlock --passphrase-stdin 2>&1"
    )
    assert exit_code == 0, f"braid unlock failed:\n{output}"

    # btrfs balance status can exit 1 for a paused balance after remount, so
    # check the text, not the exit code.
    status = machine.execute("btrfs balance status /mnt/storage")[1]
    assert "paused" in status.lower(), (
        f"expected the balance still paused after lock and unlock, got:\n{status}"
    )
    assert "paused balance" in output, (
        f"expected unlock to warn about the paused balance:\n{output}"
    )

# Clean up
machine.succeed("btrfs balance cancel /mnt/storage")
machine.shutdown()
