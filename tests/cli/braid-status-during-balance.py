# Intent: braid status must succeed while a RAID1 balance is in progress.
# Why: data ratio is fractional during balance (e.g. "1.01"), which
#      previously caused a hard parse error.
# Scenario: user adds a single disk, writes data, manually adds a second disk,
#           starts a balance and pauses it to guarantee a stable mid-balance
#           window, then runs braid status to verify it handles the state.

import json
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

# 4. Start and pause a balance via the shared helper.
#    dm-delay keeps each relocation slow enough for the helper to catch.
with subtest("start and pause balance"):
    dm_delay_activate(machine, DELAYED_DISKS, write_delay_ms=500)
    pause_balance_with_remaining_work(machine)
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
