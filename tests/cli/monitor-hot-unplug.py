# Test: braid monitor alert lifecycle after hot-unplug
#
# Intent: Verify that hot-unplugging a disk from a live RAID1 pool triggers
#   the alert lifecycle correctly, even during the window before btrfs marks
#   the device as MISSING.
#
# Why it exists: On real hardware, hot-unplug caused braid monitor to exit 2
#   (error) instead of exit 1 (alert) because the LUKS mapper persisted with
#   device: (null). No beep fired — the core alerting promise was broken.
#   braid ack also crashed with the same error.
#
# Scenario: 3-disk RAID1 pool. Two disks are virtio, the third is a
#   scsi_debug device. The SCSI device is deleted via sysfs to faithfully
#   simulate SATA hot-unplug: /dev/sdX disappears, the LUKS dm mapper
#   stays open with device: (null), btrfs still reports the mapper path.
#   braid monitor must detect this, exit 1, and support the full alert
#   lifecycle including braid ack.

import json

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"

# --- Setup: create scsi_debug device for disk2 ---
with subtest("Create scsi_debug device for disk2"):
    # scsi_debug allocates RAM for the virtual disk; keep it small to avoid OOM.
    # LUKS2 headers need ~32 MB, so 256 MB is plenty for a btrfs pool member.
    machine.succeed("modprobe scsi_debug dev_size_mb=256 num_tgts=1 sector_size=512")
    machine.wait_until_succeeds("test -b /dev/sda", timeout=10)
    # Record the SCSI address that owns /dev/sda (the scsi_debug disk).
    # Can't blindly take the first /sys/class/scsi_device/ entry — the
    # QEMU CD-ROM (1:0:0:0) sorts before scsi_debug (2:0:0:0).
    scsi_host = machine.succeed(
        "basename $(readlink -f /sys/block/sda/device)"
    ).strip()

# --- Setup: create 3-disk RAID1 pool ---
with subtest("Create 3-disk RAID1 pool"):
    # disk1 and disk3 on virtio
    for d in ["disk1", "disk3"]:
        machine.succeed(
            f"echo -n '{passphrase}' | cryptsetup luksFormat --batch-mode --key-file=- --pbkdf pbkdf2 --pbkdf-force-iterations 1000 /dev/disk/by-id/virtio-{d}"
        )
        machine.succeed(
            f"echo -n '{passphrase}' | cryptsetup open --type luks --key-file=- /dev/disk/by-id/virtio-{d} braid-{d}"
        )

    # disk2 on scsi_debug
    machine.succeed(
        f"echo -n '{passphrase}' | cryptsetup luksFormat --batch-mode --key-file=- --pbkdf pbkdf2 --pbkdf-force-iterations 1000 /dev/sda"
    )
    machine.succeed(
        f"echo -n '{passphrase}' | cryptsetup open --type luks --key-file=- /dev/sda braid-disk2"
    )

    machine.succeed(
        "mkfs.btrfs -f -d raid1 -m raid1 /dev/mapper/braid-disk1 /dev/mapper/braid-disk2 /dev/mapper/braid-disk3"
    )
    machine.succeed("mkdir -p /mnt/storage")
    machine.succeed("mount /dev/mapper/braid-disk1 /mnt/storage")
    machine.succeed("mkdir -p /var/lib/braid")

with subtest("Healthy pool: monitor exits 0"):
    machine.succeed("braid monitor")

# --- Hot-unplug disk2 via SCSI device deletion ---
with subtest("Hot-unplug disk2 via sysfs"):
    # Delete the SCSI device — simulates SATA hot-unplug.
    machine.succeed(f"echo 1 > /sys/class/scsi_device/{scsi_host}/device/delete")
    # Don't wait for /dev/sda to vanish — dm holds a reference so the kernel
    # may defer block device removal. The authoritative gate is cryptsetup
    # reporting (null), which means the LUKS mapper can no longer reach its
    # backing device.
    machine.succeed("test -e /dev/mapper/braid-disk2")
    machine.wait_until_succeeds(
        "cryptsetup status braid-disk2 | grep '(null)'", timeout=30
    )

with subtest("Monitor detects null-underlying as missing (exit 1)"):
    rc = machine.succeed("set +e; braid monitor; echo $?").strip().splitlines()[-1]
    assert rc == "1", f"Expected exit 1, got {rc}"

with subtest("Alert latch created"):
    machine.succeed("test -f /var/lib/braid/alert-latch.json")

with subtest("Status shows ALERT with missing device"):
    output = machine.succeed("braid status")
    assert "ALERT" in output, f"Expected ALERT in status, got: {output}"
    assert "missing device" in output, f"Expected 'missing device' cause, got: {output}"

with subtest("Status JSON shows alert"):
    json_output = machine.succeed("braid status --json")
    report = json.loads(json_output)
    assert report["alert_active"] == True, f"Expected alert_active=true, got: {report}"
    cause_types = [c["type"] for c in report["alert_causes"]]
    assert "missing_device" in cause_types, f"Expected missing_device cause, got: {cause_types}"

with subtest("Ack succeeds"):
    machine.succeed("braid ack")

with subtest("Ack removes latch"):
    machine.fail("test -f /var/lib/braid/alert-latch.json")

with subtest("After ack: monitor exits 0"):
    machine.succeed("braid monitor")

machine.shutdown()
