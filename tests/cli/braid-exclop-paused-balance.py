# Intent: braid add and braid lock must fail fast when a balance is paused.
# Why: a paused balance holds the exclusive lock indefinitely — --enqueue would
#   hang forever. Braid must detect this via sysfs and error immediately.
# Scenario: operator has a 2-disk pool with a paused balance. They try to add
#   a third disk (should fail with "paused" message) and try to lock the pool
#   (should fail with "in progress" message, pool stays mounted).

import re
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

# 3. Start and pause a balance.
#    Reuse the retry pattern from braid-status-during-balance.py:56-102.
with subtest("start and pause balance"):
    targets = ["single", "raid1"]
    paused = False
    for attempt in range(3):
        target = targets[attempt % 2]

        machine.execute(
            f"btrfs balance start -dconvert={target} -mconvert={target} /mnt/storage "
            f"> /tmp/balance.log 2>&1 & "
            f"for i in $(seq 1 200); do "
            f"  btrfs balance pause /mnt/storage 2>/dev/null && break; "
            f"  sleep 0.02; "
            f"done"
        )

        ret = machine.execute("btrfs balance status /mnt/storage")
        output = ret[1]

        if "paused" in output.lower():
            match = re.search(
                r"(\d+)\s+out of about\s+(\d+)\s+chunks", output
            )
            if match and int(match.group(1)) < int(match.group(2)):
                paused = True
                break

        # Balance completed or paused with no remaining work — clean up and retry.
        machine.execute(
            "btrfs balance cancel /mnt/storage 2>/dev/null || true"
        )
        for _ in range(30):
            ret = machine.execute("btrfs balance status /mnt/storage")
            if "no balance" in ret[1].lower():
                break
            import time
            time.sleep(0.2)
        else:
            raise Exception(
                "Balance did not terminate after cancel — cannot retry safely"
            )

    assert paused, "Could not pause balance with remaining work after 3 attempts"

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

# 5. Test that braid lock also refuses.
with subtest("braid lock refuses on paused balance"):
    result = machine.execute("braid lock 2>&1")
    exit_code = result[0]
    output = result[1]

    assert exit_code != 0, (
        f"braid lock should have failed with paused balance, but exited 0:\n{output}"
    )
    assert "in progress" in output.lower(), (
        f"expected 'in progress' in error output:\n{output}"
    )

    # Pool must still be mounted (lock should not have proceeded)
    machine.succeed("mountpoint -q /mnt/storage")

    # LUKS mappers must still be open
    machine.succeed("test -e /dev/mapper/braid-disk1")
    machine.succeed("test -e /dev/mapper/braid-disk2")

# Clean up
machine.succeed("btrfs balance cancel /mnt/storage")
machine.shutdown()
