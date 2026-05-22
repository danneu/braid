# Intent: braid status must succeed while a RAID1 balance is in progress.
# Why: data ratio is fractional during balance (e.g. "1.01"), which
#      previously caused a hard parse error.
# Scenario: user adds a single disk, writes data, manually adds a second disk,
#           starts a balance and pauses it to guarantee a stable mid-balance
#           window, then runs braid status to verify it handles the state.

import json
import re
import shlex

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"
DELAYED_DISKS = ["disk1", "disk2"]


def disk_path(key):
    if key in DELAYED_DISKS:
        return f"/dev/disk/by-id/braid-test-{key}-delay"
    return f"/dev/disk/by-id/virtio-{key}"


def add_disk(key):
    passphrase_q = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {passphrase_q} | "
        f"braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 {key}={disk_path(key)} --passphrase-stdin --yes"
    )


# 1. Create single-disk pool via braid add
with subtest("create single-disk pool"):
    for name in DELAYED_DISKS:
        dm_delay_create(machine, name)
    machine.succeed(add_disk("disk1"))
    machine.succeed("mountpoint -q /mnt/storage")

# 2. Write test data so balance has observable work.
#    dm-delay, not payload size, makes the single->RAID1 rebalance observable.
with subtest("write test data"):
    machine.succeed("dd if=/dev/urandom of=/mnt/storage/bigfile bs=1M count=32")
    machine.succeed("sync")

# 3. LUKS-format and open disk2 manually, add to btrfs directly
#    (skip braid add which blocks on balance completion)
with subtest("manually add disk2 to btrfs"):
    dev2 = disk_path("disk2")
    passphrase_q = shlex.quote(passphrase)
    machine.succeed(
        f"printf '%s\\n' {passphrase_q} | "
        f"cryptsetup luksFormat --batch-mode --key-file=- "
        f"{luks_opts} {dev2}"
    )
    machine.succeed(
        f"printf '%s\\n' {passphrase_q} | "
        f"cryptsetup open --key-file=- {dev2} braid-disk2"
    )
    machine.succeed("btrfs device add /dev/mapper/braid-disk2 /mnt/storage")

# 4. Start balance and immediately pause it.
#    The balance on small VM disks completes in <2s, too fast to reliably poll.
#    Start+pause in a single shell command to avoid Python roundtrip overhead.
#    If the balance completes before pause catches it, retry with the opposite
#    conversion target so there's always new work to do.
with subtest("start and pause balance"):
    dm_delay_activate(machine, DELAYED_DISKS, write_delay_ms=500)
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
    dm_delay_deactivate(machine, DELAYED_DISKS)

# 5. With the balance reliably paused, check both text and JSON output.
with subtest("status during balance"):
    text_out = machine.succeed("braid status 2>&1")
    json_out = machine.succeed("braid status --json 2>&1")

    # Text assertions
    print(f"status during balance:\n{text_out}")
    assert "Pool:" in text_out, f"Expected 'Pool:':\n{text_out}"
    assert "Drives:" in text_out, f"Expected 'Drives:':\n{text_out}"
    assert "Balance:" in text_out, f"Expected 'Balance:' line:\n{text_out}"

    # JSON assertions
    s = json.loads(json_out)
    assert s["status"] in ("intact", "degraded"), (
        f"Expected intact or degraded: {s['status']}"
    )
    assert "balance" in s, f"Expected 'balance' key in JSON: {s}"
    assert s["balance"]["state"] == "paused", f"Expected paused balance: {s['balance']}"

# Clean up: cancel the paused balance
machine.succeed("btrfs balance cancel /mnt/storage")
machine.shutdown()
