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

machine.succeed(
    f"btrfs replace status -1 {MOUNT}"
    f" > {FIXTURE_DIR}/btrfs-replace-status-never-started.txt"
)

# Write some data so usage stats are non-trivial
machine.succeed(f"dd if=/dev/urandom of={MOUNT}/testfile bs=1M count=16")
machine.succeed("sync")

# --- Capture fixtures ---

# 1. lsblk (JSON) — filter to the disks we set up
# Keep the --output list in sync with CmdRequest::LsblkJson in cli/src/cmd.rs.
machine.succeed(
    f"lsblk --json --bytes --output NAME,TYPE,SIZE,MODEL,SERIAL,UUID,ROTA,TRAN /dev/vdb /dev/vdc"
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

# 5b. btrfs device stats (text)
machine.succeed(
    f"btrfs device stats {MOUNT}"
    f" > {FIXTURE_DIR}/btrfs-device-stats-2disk.txt"
)

# 6. btrfs scrub status — before any scrub (should say "no stats available")
machine.succeed(
    f"btrfs scrub status --raw {MOUNT}"
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

# 8b. cryptsetup luksDump --dump-json-metadata
machine.succeed(
    f"cryptsetup luksDump --dump-json-metadata /dev/vdb"
    f" > {FIXTURE_DIR}/cryptsetup-luks-dump.json"
)

# 8c. cryptsetup luksDump (text)
# Used by parse_cryptsetup_luks_label and parse_cryptsetup_luks_version.
# The version parser is the gateway check that enforces braid's
# LUKS2-only invariant in probe_config_disk and discover.rs.
machine.succeed(
    f"cryptsetup luksDump /dev/vdb"
    f" > {FIXTURE_DIR}/cryptsetup-luks-dump.txt"
)

# 10. Run a scrub, then capture completed status
machine.succeed(f"btrfs scrub start -B {MOUNT}")
machine.succeed(
    f"btrfs scrub status --raw {MOUNT}"
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

# 14b. btrfs subvolume list (requires creating subvolumes first)
machine.succeed(f"btrfs subvolume create {MOUNT}/data")
machine.succeed(f"btrfs subvolume create {MOUNT}/snapshots")
machine.succeed(
    f"btrfs subvolume list {MOUNT}"
    f" > {FIXTURE_DIR}/btrfs-subvolume-list.txt"
)

# 14. btrfs balance status (paused after skip_balance remount)
# Captures the exact output btrfs-progs produces when counters are reset to 0/0.
# This is the canary for formatting drift (e.g. nan vs -nan).
import base64
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

# Rebuild a clean filesystem for replace captures. The balance fixture above
# intentionally leaves mixed data profiles behind, and that topology can make
# dev_replace fail with ENOSPC before the canceled fixture observes an
# in-flight state.
machine.succeed(
    "mkfs.btrfs -f -d raid1 -m raid1 /dev/mapper/braid-vdb /dev/mapper/braid-vdc"
)
machine.succeed(f"mount /dev/mapper/braid-vdb {MOUNT}")

# Format the third disk as LUKS, open as braid-vdd.
machine.succeed(
    f"echo -n '{PASSPHRASE}' | cryptsetup luksFormat --batch-mode /dev/vdd -"
)
machine.succeed(f"echo -n '{PASSPHRASE}' | cryptsetup open /dev/vdd braid-vdd -")

# Write a large payload so `btrfs replace` runs long enough to observe
# an in-flight window before cancel. The post-balance-cleanup filesystem
# has only ~16 MiB of user data; a replace on that scale can finish faster
# than the 0.05s polling cadence below, leading to a captured "finished"
# output instead of "canceled". Keep this below the earlier balance payload so
# the 1 GiB fixture pool still has room for btrfs to prepare dev_replace.
machine.succeed(f"dd if=/dev/urandom of={MOUNT}/replacedata bs=1M count=256")
machine.succeed("sync")

# Capture canceled: start replace vdb -> vdd in background, hard-assert
# in-flight observation before cancel.
PCT_RE = re.compile(r"(\d+(?:\.\d+)?)% done")


def parse_replace_state(text):
    if "finished on" in text:
        return ("finished", 100.0)
    match = PCT_RE.search(text)
    if match:
        return ("running", float(match.group(1)))
    return ("idle", None)


machine.execute(
    f"btrfs replace start -B 1 /dev/mapper/braid-vdd {MOUNT} "
    f"> /tmp/btrfs-replace-start.log 2>&1 &"
)
saw_in_flight = False
saw_finished_too_early = False
last_status = ""
for _ in range(800):  # 40s budget
    ret = machine.execute(f"btrfs replace status -1 {MOUNT} 2>&1")
    last_status = ret[1]
    state, _ = parse_replace_state(last_status)
    if state == "running":
        saw_in_flight = True
        break
    if state == "finished":
        saw_finished_too_early = True
        break
    time.sleep(0.05)
assert not saw_finished_too_early, (
    "btrfs replace finished before the canceled fixture could observe "
    "in-flight state. Payload too small or polling cadence too coarse. "
    "Last status:\n" + last_status
)
assert saw_in_flight, (
    "Never observed btrfs replace in-flight -- canceled fixture cannot "
    "be captured deterministically. Last status:\n" + last_status
)
last_status_b64 = base64.b64encode(last_status.encode()).decode()
machine.succeed(
    f"printf %s {last_status_b64} | base64 -d"
    f" > {FIXTURE_DIR}/btrfs-replace-status-running.txt"
)

# `btrfs replace cancel` returns once scrub cancel is requested, but the
# kernel's CANCELED state transition runs in `btrfs_dev_replace_finishing`
# (dev-replace.c:937-939). Status can still report running for a tick before
# the flip. Poll until "canceled on" appears; hard-fail on timeout or any
# unexpected state.
machine.succeed(f"btrfs replace cancel {MOUNT}")
saw_canceled = False
saw_finished_too_early = False
last_status = ""
for _ in range(400):  # 20s budget
    ret = machine.execute(f"btrfs replace status -1 {MOUNT} 2>&1")
    last_status = ret[1]
    if "canceled on" in last_status:
        saw_canceled = True
        break
    if "finished on" in last_status:
        saw_finished_too_early = True
        break
    time.sleep(0.05)
assert not saw_finished_too_early, (
    "btrfs replace transitioned to FINISHED after cancel -- the cancel raced "
    "kernel completion and the canceled fixture cannot be captured. Last "
    "status:\n" + last_status
)
assert saw_canceled, (
    "Kernel never transitioned to CANCELED within budget. Last status:\n"
    + last_status
)
machine.succeed(
    f"btrfs replace status -1 {MOUNT}"
    f" > {FIXTURE_DIR}/btrfs-replace-status-canceled.txt"
)

# Capture finished: rerun replace to completion. The previous tgtdev
# allocation was destroyed on cancel; pass -f because braid-vdd may carry
# residual fs signatures from the canceled run. -B blocks until the kernel
# reports finished, so no in-flight observation needed here.
machine.succeed(f"btrfs replace start -B -f 1 /dev/mapper/braid-vdd {MOUNT}")
machine.succeed(
    f"btrfs replace status -1 {MOUNT}"
    f" > {FIXTURE_DIR}/btrfs-replace-status-finished.txt"
)

# Remove the payload before remaining teardown.
machine.succeed(f"rm {MOUNT}/replacedata")

# --- Degraded device stats: drop one member, capture the missing-device row ---
# Rebuild a clean 2-disk pool on the open mappers, then close one member and
# mount degraded. The parser ignores the btrfs-emitted device string and keys
# on devid, so this fixture pins the real degraded JSON shape.
machine.succeed(f"umount {MOUNT}")
machine.succeed(
    "mkfs.btrfs -f -d raid1 -m raid1 /dev/mapper/braid-vdb /dev/mapper/braid-vdc"
)
machine.succeed(f"mount /dev/mapper/braid-vdb {MOUNT}")
# Write data on the healthy mount so btrfs allocates a Data,RAID1 chunk on
# both members -- chunk allocation is lazy, so a fresh mkfs has no Data row.
# The missing-device usage capture below depends on devid 2 still tracking
# Data/Metadata/System RAID1 rows after braid-vdc is closed.
machine.succeed(f"dd if=/dev/urandom of={MOUNT}/degradeddata bs=1M count=16")
machine.succeed("sync")
machine.succeed(f"umount {MOUNT}")
machine.succeed("cryptsetup close braid-vdc")
machine.succeed(f"mount -o degraded /dev/mapper/braid-vdb {MOUNT}")
machine.succeed(
    f"btrfs --format json device stats {MOUNT}"
    f" > {FIXTURE_DIR}/btrfs-device-stats-degraded.json"
)
machine.succeed(
    f"btrfs device stats {MOUNT}"
    f" > {FIXTURE_DIR}/btrfs-device-stats-degraded.txt"
)
# Capture the missing-device `device usage --raw` stanza while still mounted
# degraded: the kernel-sourced path renders as `<missing disk>, ID: 2` with
# Device size 0 but keeps its RAID1 allocation rows. Pins the shape
# check_raid1_relocation_space reads.
machine.succeed(
    f"btrfs device usage --raw {MOUNT}"
    f" > {FIXTURE_DIR}/btrfs-device-usage-missing.txt"
)
machine.succeed(f"umount {MOUNT}")

# 11. cryptsetup status (inactive stderr/stdout)
# Must unmount before closing mapper; otherwise cryptsetup reports "still in use".
machine.succeed("cryptsetup close braid-vdb")
machine.succeed(
    f"cryptsetup status braid-vdb"
    f" > {FIXTURE_DIR}/cryptsetup-status-inactive.stdout"
    f" 2> {FIXTURE_DIR}/cryptsetup-status-inactive.stderr || true"
)

# --- Copy fixtures out of the VM ---
machine.copy_from_vm(FIXTURE_DIR, "")
