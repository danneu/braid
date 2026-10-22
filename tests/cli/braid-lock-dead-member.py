# Test: braid-lock-dead-member
#
# Intent: Verify `braid lock` closes a member's LUKS mapper whose backing disk
#   is gone, and reports it as that member.
#
# Why it exists: lock proved a mapper's owner by rereading the LUKS header
#   through the backing disk (`cryptsetup status`, then `cryptsetup luksUUID`).
#   A gone or dying disk made that impossible, so lock warned, skipped the
#   mapper, and left it open with its volume key in kernel memory -- the case
#   where locking matters most (finding 261021-1103-013).
#
# Scenario: 2-disk RAID1 pool; disk2 is a scsi_debug device. The operator
#   unmounts the pool, disk2 dies (its SCSI device is deleted, so cryptsetup
#   reports `device: (null)`), and the operator runs `braid lock` before pulling
#   the disk.

import json
import shlex

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"

with subtest("Create a scsi_debug device for disk2"):
    # scsi_debug allocates RAM for the disk; 256 MB holds a LUKS2 header and a
    # small btrfs member.
    machine.succeed("modprobe scsi_debug dev_size_mb=256 num_tgts=1 sector_size=512")
    machine.wait_until_succeeds("test -b /dev/sda", timeout=10)
    # The QEMU CD-ROM sorts before scsi_debug, so read the SCSI address that
    # owns /dev/sda instead of taking the first /sys/class/scsi_device entry.
    scsi_host = machine.succeed("basename $(readlink -f /sys/block/sda/device)").strip()

with subtest("Build a mounted two-disk pool and its pool.json"):
    devices = {
        "disk1": "/dev/disk/by-id/virtio-disk1",
        "disk2": "/dev/sda",
    }
    members = {}
    for name, device in devices.items():
        machine.succeed(
            f"echo -n '{passphrase}' | cryptsetup luksFormat --batch-mode --key-file=- "
            f"--pbkdf pbkdf2 --pbkdf-force-iterations 1000 {device}"
        )
        machine.succeed(
            f"echo -n '{passphrase}' | cryptsetup open --type luks --key-file=- "
            f"{device} braid-{name}"
        )
        luks_uuid = machine.succeed(f"cryptsetup luksUUID {device}").strip()
        members[luks_uuid] = {
            "name": name,
            "by_id": f"/dev/disk/by-id/test-{name}",
        }
    machine.succeed(
        "mkfs.btrfs -f -d raid1 -m raid1 /dev/mapper/braid-disk1 /dev/mapper/braid-disk2"
    )
    machine.succeed("mkdir -p /mnt/storage /var/lib/braid")
    machine.succeed("mount /dev/mapper/braid-disk1 /mnt/storage")
    pool_json = json.dumps({"disks": members}, sort_keys=True)
    machine.succeed(f"printf '%s' {shlex.quote(pool_json)} > /var/lib/braid/pool.json")

with subtest("Unmount the pool, then disk2 dies"):
    machine.succeed("umount /mnt/storage")
    machine.succeed(f"echo 1 > /sys/class/scsi_device/{scsi_host}/device/delete")
    # dm holds a reference, so /dev/sda may linger; cryptsetup reporting (null)
    # is the gate that the mapper can no longer reach its backing disk.
    machine.wait_until_succeeds("cryptsetup status braid-disk2 | grep -F '(null)'", timeout=30)
    machine.fail("cryptsetup luksUUID /dev/sda")
    machine.succeed("test -e /dev/mapper/braid-disk2")

with subtest("Lock closes the dead member's mapper as disk2"):
    status, output = machine.execute("braid lock 2>&1")
    assert "disk disk2: locked" in output, "disk2 must close as a member: " + output
    assert "skipping mapper" not in output, output
    assert "disk disk1: locked" in output, output
    assert status == 0, "braid lock failed: " + output
    machine.fail("test -e /dev/mapper/braid-disk2")
    machine.fail("test -e /dev/mapper/braid-disk1")

machine.shutdown()
