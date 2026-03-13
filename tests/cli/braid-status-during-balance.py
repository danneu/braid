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

# 2. Write ~2 GiB so balance has observable work
with subtest("write test data"):
    machine.succeed("dd if=/dev/urandom of=/mnt/storage/bigfile bs=1M count=2048")
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

# 5. Poll until balance is confirmed running
with subtest("wait for balance to be running"):
    machine.succeed(
        "for i in $(seq 1 2400); do "
        'out="$(btrfs balance status /mnt/storage 2>&1 || true)"; '
        "if printf '%s\\n' \"$out\" | grep -Eq 'is (running|paused)'; then "
        "exit 0; fi; sleep 0.05; done; exit 1"
    )

# 6. Run braid status while balance is in progress
with subtest("status during balance"):
    output = machine.succeed("braid status")
    print(f"status during balance:\n{output}")
    assert "Pool:" in output, f"Expected 'Pool:':\n{output}"
    assert "Drives:" in output, f"Expected 'Drives:':\n{output}"

with subtest("json status during balance"):
    raw = machine.succeed("braid status --json")
    s = json.loads(raw)
    assert s["status_code"] in ("healthy", "degraded"), (
        f"Expected healthy or degraded: {s['status_code']}"
    )

machine.shutdown()
