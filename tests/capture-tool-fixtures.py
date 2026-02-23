import os

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

# 5. btrfs device stats (text)
machine.succeed(
    f"btrfs device stats {MOUNT}"
    f" > {FIXTURE_DIR}/btrfs-device-stats-2disk.txt"
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

# --- Copy fixtures out of the VM ---
machine.copy_from_vm(FIXTURE_DIR, "")
