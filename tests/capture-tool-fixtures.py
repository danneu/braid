# Set up LUKS + btrfs RAID1 on two virtual disks, then capture every command
# output that the Rust parsers consume. Each output is written to /tmp/fixtures/
# and copied out via copy_from_vm for use as golden-file test fixtures.

PASSPHRASE = "test-passphrase"
MOUNT = "/mnt/storage"
FIXTURE_DIR = "/tmp/fixtures"

start_all()
machine.wait_for_unit("multi-user.target")

machine.succeed(f"mkdir -p {FIXTURE_DIR}")

# --- Set up LUKS on both disks ---
for disk in ["vdb", "vdc"]:
    machine.succeed(
        f"echo -n '{PASSPHRASE}' | cryptsetup luksFormat --batch-mode /dev/{disk} -"
    )
    machine.succeed(
        f"echo -n '{PASSPHRASE}' | cryptsetup open /dev/{disk} braid-{disk} -"
    )

# --- Create btrfs RAID1 ---
machine.succeed(
    "mkfs.btrfs -f -d raid1 -m raid1 /dev/mapper/braid-vdb /dev/mapper/braid-vdc"
)
machine.succeed(f"mkdir -p {MOUNT}")
machine.succeed(f"mount /dev/mapper/braid-vdb {MOUNT}")

# Write some data so usage stats are non-trivial
machine.succeed(f"dd if=/dev/urandom of={MOUNT}/testfile bs=1M count=16")
machine.succeed("sync")

# --- Capture fixtures ---

# 1. lsblk (JSON) — filter to the disks we set up
machine.succeed(
    f"lsblk --json --bytes --output NAME,TYPE,SIZE,MODEL,SERIAL,UUID /dev/vdb /dev/vdc"
    f" > {FIXTURE_DIR}/lsblk-2disk.json"
)

# 2. btrfs filesystem df (JSON)
machine.succeed(
    f"btrfs --format json filesystem df {MOUNT}"
    f" > {FIXTURE_DIR}/btrfs-df-raid1.json"
)

# 3. btrfs filesystem show (text)
machine.succeed(
    f"btrfs filesystem show {MOUNT}"
    f" > {FIXTURE_DIR}/btrfs-show-2disk.txt"
)

# 4. btrfs filesystem usage --raw (text)
machine.succeed(
    f"btrfs filesystem usage --raw {MOUNT}"
    f" > {FIXTURE_DIR}/btrfs-usage-raw.txt"
)

# 5. btrfs device stats (JSON)
machine.succeed(
    f"btrfs --format json device stats {MOUNT}"
    f" > {FIXTURE_DIR}/btrfs-device-stats-2disk.json"
)

# 6. btrfs scrub status — before any scrub (should say "no stats available")
machine.succeed(
    f"btrfs scrub status {MOUNT}"
    f" > {FIXTURE_DIR}/btrfs-scrub-never.txt"
)

# 7. cryptsetup status (text)
machine.succeed(
    f"cryptsetup status braid-vdb"
    f" > {FIXTURE_DIR}/cryptsetup-status-active.txt"
)

# 8. cryptsetup luksUUID (text)
machine.succeed(
    f"cryptsetup luksUUID /dev/vdb"
    f" > {FIXTURE_DIR}/cryptsetup-luks-uuid.txt"
)

# 9. findmnt (JSON)
machine.succeed(
    f"findmnt --json --output TARGET,SOURCE,FSTYPE --mountpoint {MOUNT}"
    f" > {FIXTURE_DIR}/findmnt-btrfs.json"
)

# 10. Run a scrub, then capture completed status
machine.succeed(f"btrfs scrub start -B {MOUNT}")
machine.succeed(
    f"btrfs scrub status {MOUNT}"
    f" > {FIXTURE_DIR}/btrfs-scrub-completed.txt"
)

# 12. btrfs balance status (idle — no balance running)
machine.succeed(
    f"btrfs balance status {MOUNT}"
    f" > {FIXTURE_DIR}/btrfs-balance-status-none.txt"
)

# 13. btrfs device usage --raw (per-device allocation breakdown)
machine.succeed(
    f"btrfs device usage --raw {MOUNT}"
    f" > {FIXTURE_DIR}/btrfs-device-usage-2disk.txt"
)

# 14. btrfs balance status (paused after skip_balance remount)
# Captures the exact output btrfs-progs produces when counters are reset to 0/0.
# This is the canary for formatting drift (e.g. nan vs -nan).
import re
import time

machine.succeed(f"dd if=/dev/urandom of={MOUNT}/balancedata bs=1M count=512")
machine.succeed("sync")

# Bounded retry: start balance → pause → check for remaining work.
# Reuses the proven pattern from tests/cli/braid-unlock.py.
targets = ["single", "raid1"]
for attempt in range(3):
    target = targets[attempt % 2]

    machine.execute(
        f"btrfs balance start -dconvert={target} {MOUNT} "
        f"> /dev/null 2>&1 & "
        f"for i in $(seq 1 200); do "
        f"  btrfs balance pause {MOUNT} 2>/dev/null && break; "
        f"  sleep 0.02; "
        f"done"
    )

    ret = machine.execute(f"btrfs balance status {MOUNT}")
    output = ret[1]

    if "paused" in output.lower():
        match = re.search(
            r"(\d+)\s+out of about\s+(\d+)\s+chunks", output
        )
        if match and int(match.group(1)) < int(match.group(2)):
            break

    machine.execute(f"btrfs balance cancel {MOUNT} 2>/dev/null || true")
    for _ in range(30):
        ret = machine.execute(f"btrfs balance status {MOUNT}")
        if "no balance" in ret[1].lower():
            break
        time.sleep(0.2)
    else:
        raise Exception(
            "Balance did not terminate after cancel — cannot retry safely"
        )
else:
    raise Exception(
        "Could not pause balance with remaining work after 3 full attempts"
    )

# Unmount and remount with skip_balance to reset kernel counters to 0/0.
machine.succeed(f"umount {MOUNT}")
machine.succeed(f"mount -o skip_balance /dev/mapper/braid-vdb {MOUNT}")

machine.execute(
    f"btrfs balance status {MOUNT}"
    f" > {FIXTURE_DIR}/btrfs-balance-status-paused-skip-balance.txt"
)

# Clean up: cancel balance, remove data, remount normally for remaining teardown.
machine.succeed(f"btrfs balance cancel {MOUNT}")
machine.succeed(f"rm {MOUNT}/balancedata")
machine.succeed(f"umount {MOUNT}")
machine.succeed(f"mount /dev/mapper/braid-vdb {MOUNT}")

# 11. cryptsetup status (inactive stderr/stdout)
# Must unmount before closing mapper; otherwise cryptsetup reports "still in use".
machine.succeed(f"umount {MOUNT}")
machine.succeed("cryptsetup close braid-vdb")
machine.succeed(
    f"cryptsetup status braid-vdb"
    f" > {FIXTURE_DIR}/cryptsetup-status-inactive.stdout"
    f" 2> {FIXTURE_DIR}/cryptsetup-status-inactive.stderr || true"
)

# --- Copy fixtures out of the VM ---
machine.copy_from_vm(FIXTURE_DIR, "")
