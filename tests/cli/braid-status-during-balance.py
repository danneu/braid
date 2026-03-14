# Intent: braid status must succeed while a RAID1 balance is in progress.
# Why: data ratio is fractional during balance (e.g. "1.01"), which
#      previously caused a hard parse error.
# Scenario: user adds a single disk, writes data, manually adds a second disk
#           and starts a balance, then runs braid status while balance runs.

import json
import shlex

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def add_disk(key):
    passphrase_q = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {passphrase_q} | "
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid add {key} --passphrase-stdin --yes"
    )


# 1. Create single-disk pool via braid add
with subtest("create single-disk pool"):
    machine.succeed(add_disk("disk1"))
    machine.succeed("mountpoint -q /mnt/storage")

# 2. Write ~512 MiB so balance has observable work.
#    Only 1/4 of disk size — more causes ENOSPC during single→RAID1 rebalance
#    because btrfs needs unallocated chunk space on the source device.
with subtest("write test data"):
    machine.succeed("dd if=/dev/urandom of=/mnt/storage/bigfile bs=1M count=512")
    machine.succeed("sync")

# 3. LUKS-format and open disk2 manually, add to btrfs directly
#    (skip braid add which blocks on balance completion)
with subtest("manually add disk2 to btrfs"):
    dev2 = "/dev/disk/by-id/virtio-disk2"
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

# 4. Start balance in background
with subtest("start balance in background"):
    machine.succeed(
        "btrfs balance start -dconvert=raid1 -mconvert=raid1 /mnt/storage "
        "> /tmp/balance.log 2>&1 < /dev/null &"
    )

# 5. Poll braid status until we catch a running balance.
#    Single loop captures both text and JSON in the same iteration to avoid the
#    race where two sequential subtests compete for one short-lived balance window.
with subtest("status during balance"):
    raw = machine.succeed(
        "for i in $(seq 1 300); do "
        'text="$(braid status 2>&1)"; '
        'js="$(braid status --json 2>&1)"; '
        "if printf '%s\\n' \"$js\" | jq -e '.balance.state == \"running\"' >/dev/null 2>&1; then "
        "printf '%s\\n' \"$text\"; "
        "printf '%s\\n' '**JSON**'; "
        "printf '%s\\n' \"$js\"; "
        "exit 0; fi; "
        "sleep 0.05; done; exit 1"
    )
    text_out, json_out = raw.split("**JSON**\n", 1)

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
    assert s["balance"]["state"] == "running", f"Expected running balance: {s['balance']}"

machine.shutdown()
