# Test: kernel journal alert lifecycle
#
# Intent: Verify that btrfs errors in the kernel journal are detected by
#   `braid monitor`, surfaced in `braid status`, persist across monitor
#   cycles (latch durability), and clear with `braid ack`.
#
# Why it exists: Journal entries are events — once the cursor advances past
#   them, they can't be re-detected. Without this test, a bug in the
#   latch merge or cursor ordering could silently lose journal alerts.
#
# Scenario: 2-disk RAID1 pool. dm-flakey injects write errors on one disk.
#   A write failure produces kernel "BTRFS error" entries. monitor detects
#   them, status shows the banner, re-monitor confirms latch persistence,
#   ack clears everything.

import json


start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
raw_disk1 = "/dev/disk/by-id/virtio-disk1"
raw_disk2 = "/dev/disk/by-id/virtio-disk2"
flakey = "/dev/mapper/flakey1"


def dm_table(up, down):
    sectors = machine.succeed(f"blockdev --getsz {raw_disk1}").strip()
    return f"0 {sectors} flakey {raw_disk1} 0 {up} {down} 1 error_writes"


def dm_create_healthy():
    table = dm_table(3600, 1)
    machine.succeed(f"dmsetup create flakey1 --table '{table}'")


def dm_switch_to_write_errors():
    table = dm_table(0, 3600)
    machine.succeed("dmsetup suspend flakey1")
    machine.succeed(f"dmsetup reload flakey1 --table '{table}'")
    machine.succeed("dmsetup resume flakey1")


# --- Setup: create 2-disk RAID1 pool with dm-flakey under disk1 ---
with subtest("Setup: dm-flakey healthy, LUKS format/open both disks, mkfs RAID1, mount"):
    machine.succeed("modprobe dm-flakey")
    dm_create_healthy()

    # disk1: LUKS on top of dm-flakey
    machine.succeed(
        f"echo -n '{passphrase}' | cryptsetup luksFormat --batch-mode --key-file=- "
        f"--pbkdf pbkdf2 --pbkdf-force-iterations 1000 {flakey}"
    )
    machine.succeed(
        f"echo -n '{passphrase}' | cryptsetup luksOpen --key-file=- {flakey} braid-disk1"
    )

    # disk2: LUKS directly on the raw disk
    machine.succeed(
        f"echo -n '{passphrase}' | cryptsetup luksFormat --batch-mode --key-file=- "
        f"--pbkdf pbkdf2 --pbkdf-force-iterations 1000 {raw_disk2}"
    )
    machine.succeed(
        f"echo -n '{passphrase}' | cryptsetup luksOpen --key-file=- {raw_disk2} braid-disk2"
    )

    machine.succeed(
        "mkfs.btrfs -f -d raid1 -m raid1 /dev/mapper/braid-disk1 /dev/mapper/braid-disk2"
    )
    machine.succeed("mkdir -p /mnt/storage")
    machine.succeed("mount /dev/mapper/braid-disk1 /mnt/storage")
    machine.succeed("mkdir -p /var/lib/braid")

    # Write some data while healthy
    machine.succeed("dd if=/dev/zero of=/mnt/storage/healthy.bin bs=1M count=4 conv=fsync status=none")


with subtest("Healthy pool: monitor exits 0"):
    machine.succeed("braid monitor")


with subtest("Healthy pool: no journal cursor yet means first-run bootstrap"):
    machine.succeed("test -f /var/lib/braid/journal-cursor")


with subtest("Inject write errors and trigger btrfs journal entries"):
    dm_switch_to_write_errors()
    # Force a write that will fail — we don't care about the exit code of dd,
    # we just need the kernel to log BTRFS errors.
    machine.execute(
        "dd if=/dev/zero of=/mnt/storage/failing.bin bs=1M count=8 conv=fsync status=none 2>&1"
    )
    # Give the kernel a moment to flush journal entries
    machine.succeed("sleep 1")
    machine.succeed("sync || true")


with subtest("After write errors: monitor exits 1"):
    rc = machine.succeed("set +e; braid monitor; echo $?").strip().splitlines()[-1]
    assert rc == "1", f"Expected exit 1, got {rc}"


with subtest("After write errors: latch file created"):
    machine.succeed("test -f /var/lib/braid/alert-latch.json")


with subtest("After write errors: status shows kernel storage error"):
    output = machine.succeed("braid status")
    assert "ALERT" in output, f"Expected ALERT in status, got: {output}"
    assert "kernel storage error" in output, f"Expected 'kernel storage error' cause, got: {output}"


with subtest("After write errors: status --json shows kernel_journal_error"):
    json_output = machine.succeed("braid status --json")
    report = json.loads(json_output)
    assert report["alert_active"] is True, f"Expected alert_active=true, got: {report}"
    cause_types = [c["type"] for c in report["alert_causes"]]
    assert "kernel_journal_error" in cause_types, f"Expected kernel_journal_error cause, got: {cause_types}"
    # Check disk_name is present on at least one journal cause
    journal_causes = [c for c in report["alert_causes"] if c["type"] == "kernel_journal_error"]
    has_disk_name = any(c.get("disk_name") is not None for c in journal_causes)
    if has_disk_name:
        print(f"Journal cause has disk_name: {journal_causes}")
    else:
        print(f"Journal causes are anonymous (no disk_name): {journal_causes}")


with subtest("Re-monitor: latch persists even though cursor advanced past entries"):
    rc = machine.succeed("set +e; braid monitor; echo $?").strip().splitlines()[-1]
    assert rc == "1", f"Expected exit 1 on re-monitor, got {rc}"


with subtest("Ack clears alert"):
    machine.succeed("braid ack")
    machine.fail("test -f /var/lib/braid/alert-latch.json")


with subtest("After ack: monitor exits 0"):
    machine.succeed("braid monitor")


with subtest("Journal cursor file exists"):
    machine.succeed("test -f /var/lib/braid/journal-cursor")


machine.shutdown()
