# Test: LUKS header auto-backup on init-disk, corrupt header restore + data recovery
#
# Verifies:
# 1. init-disk creates header backup files at /var/lib/braid/luks-headers/
# 2. Backup directory has 0700, backup files have 0600 permissions
# 3. Backup UUID matches device UUID
# 4. A corrupted LUKS header can be restored from backup
# 5. Data written to a btrfs RAID1 pool survives header corruption + restore

import json

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"
backup_dir = "/var/lib/braid/luks-headers"


def write_config(disk_list, mount="/mnt/storage"):
    config = json.dumps({"disks": disk_list, "mountPoint": mount})
    escaped = config.replace("'", "'\\''")
    return f"echo '{escaped}' > /tmp/braid-config.json"


def init_disk(by_id):
    env = f"BRAID_PASSPHRASE='{passphrase}' BRAID_LUKS_OPTS='{luks_opts}'"
    return f"{env} braid init-disk --config /tmp/braid-config.json {by_id}"


# ============================================================================
# Phase 1: init-disk creates header backups
# ============================================================================

with subtest("init-disk creates header backups for both disks"):
    machine.succeed(write_config([
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk2",
    ]))
    machine.succeed(init_disk("/dev/disk/by-id/virtio-disk1"))
    machine.succeed(init_disk("/dev/disk/by-id/virtio-disk2"))

with subtest("backup files exist"):
    machine.succeed(f"test -f {backup_dir}/virtio-disk1.img")
    machine.succeed(f"test -f {backup_dir}/virtio-disk2.img")

with subtest("backup directory has 0700 permissions"):
    perms = machine.succeed(f"stat -c '%a' {backup_dir}").strip()
    assert perms == "700", f"expected 700, got {perms}"

with subtest("backup files have 0600 permissions"):
    for disk in ["virtio-disk1", "virtio-disk2"]:
        perms = machine.succeed(f"stat -c '%a' {backup_dir}/{disk}.img").strip()
        assert perms == "600", f"expected 600 for {disk}.img, got {perms}"

with subtest("backup UUID matches device UUID"):
    for disk in ["virtio-disk1", "virtio-disk2"]:
        device_uuid = machine.succeed(
            f"cryptsetup luksUUID /dev/disk/by-id/{disk}"
        ).strip()
        backup_uuid = machine.succeed(
            f"cryptsetup luksUUID --header {backup_dir}/{disk}.img /dev/disk/by-id/{disk}"
        ).strip()
        assert device_uuid == backup_uuid, (
            f"UUID mismatch for {disk}: device={device_uuid}, backup={backup_uuid}"
        )

# ============================================================================
# Phase 2: Set up pool and write test data
# ============================================================================

with subtest("create btrfs RAID1 pool with test data"):
    machine.succeed(
        f"echo -n '{passphrase}' | cryptsetup luksOpen --key-file=- "
        "/dev/disk/by-id/virtio-disk1 virtio-disk1"
    )
    machine.succeed(
        f"echo -n '{passphrase}' | cryptsetup luksOpen --key-file=- "
        "/dev/disk/by-id/virtio-disk2 virtio-disk2"
    )
    machine.succeed("mkfs.btrfs -f -d raid1 -m raid1 /dev/mapper/virtio-disk1 /dev/mapper/virtio-disk2")
    machine.succeed("mkdir -p /mnt/storage")
    machine.succeed("mount /dev/mapper/virtio-disk1 /mnt/storage")
    machine.succeed("echo 'important data survives header corruption' > /mnt/storage/testfile.txt")
    machine.succeed("sync")

# ============================================================================
# Phase 3: Corrupt disk1 LUKS header, verify it's broken
# ============================================================================

with subtest("corrupt disk1 LUKS header"):
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup close virtio-disk1")
    machine.succeed("cryptsetup close virtio-disk2")
    # Overwrite LUKS header (first 4MB)
    machine.succeed("dd if=/dev/zero of=/dev/disk/by-id/virtio-disk1 bs=4096 count=1024")

with subtest("disk1 is no longer recognized as LUKS"):
    machine.fail("cryptsetup isLuks /dev/disk/by-id/virtio-disk1")

# ============================================================================
# Phase 4: Restore header from backup, verify data recovery
# ============================================================================

with subtest("restore LUKS header from backup"):
    machine.succeed(
        f"cryptsetup luksHeaderRestore "
        f"--header-backup-file {backup_dir}/virtio-disk1.img "
        "/dev/disk/by-id/virtio-disk1"
    )

with subtest("disk1 is LUKS again"):
    machine.succeed("cryptsetup isLuks /dev/disk/by-id/virtio-disk1")

with subtest("unlock with original passphrase and verify data"):
    machine.succeed(
        f"echo -n '{passphrase}' | cryptsetup luksOpen --key-file=- "
        "/dev/disk/by-id/virtio-disk1 virtio-disk1"
    )
    machine.succeed(
        f"echo -n '{passphrase}' | cryptsetup luksOpen --key-file=- "
        "/dev/disk/by-id/virtio-disk2 virtio-disk2"
    )
    machine.succeed("mount -o degraded /dev/mapper/virtio-disk1 /mnt/storage")
    content = machine.succeed("cat /mnt/storage/testfile.txt").strip()
    assert content == "important data survives header corruption", (
        f"data mismatch: got '{content}'"
    )

# Clean up
machine.succeed("umount /mnt/storage || true")
machine.succeed("cryptsetup close virtio-disk1 || true")
machine.succeed("cryptsetup close virtio-disk2 || true")

machine.shutdown()
