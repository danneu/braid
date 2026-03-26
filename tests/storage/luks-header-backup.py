# Test: LUKS header auto-backup on braid add, corrupt header restore + data recovery
#
# What: Verifies that `braid add` automatically backs up LUKS headers, and that
# a corrupted header can be restored from backup to recover data.
#
# Why: LUKS header corruption means permanent data loss regardless of knowing
# the passphrase. `braid add` is the only luksFormat path (Principle 3), so
# auto-backup here guarantees every formatted disk has a recoverable header.
#
# Dependencies: LUKS primitives, btrfs basics, Rust braid binary with add command.

start_all()
machine.wait_for_unit("multi-user.target")

import shlex

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"
backup_dir = "/var/lib/braid/luks-headers"


def add_disk(key):
    passphrase_q = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {passphrase_q} | "
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid add {key}=/dev/disk/by-id/virtio-{key} --passphrase-stdin --yes"
    )


# ============================================================================
# Phase 1: braid add creates header backups
# ============================================================================

with subtest("braid add creates header backups for both disks"):
    machine.succeed(add_disk("disk1"))
    machine.succeed(add_disk("disk2"))

with subtest("backup files exist"):
    machine.succeed(f"test -f {backup_dir}/braid-disk1.luksheader")
    machine.succeed(f"test -f {backup_dir}/braid-disk2.luksheader")

with subtest("backup directory has 0700 permissions"):
    perms = machine.succeed(f"stat -c '%a' {backup_dir}").strip()
    assert perms == "700", f"expected 700, got {perms}"

with subtest("backup files have 0400 permissions"):
    for disk in ["braid-disk1", "braid-disk2"]:
        perms = machine.succeed(f"stat -c '%a' {backup_dir}/{disk}.luksheader").strip()
        assert perms == "400", f"expected 400 for {disk}.luksheader, got {perms}"

with subtest("backup UUID matches device UUID"):
    for name, by_id in [("braid-disk1", "virtio-disk1"), ("braid-disk2", "virtio-disk2")]:
        device_uuid = machine.succeed(
            f"cryptsetup luksUUID /dev/disk/by-id/{by_id}"
        ).strip()
        backup_uuid = machine.succeed(
            f"cryptsetup luksUUID --header {backup_dir}/{name}.luksheader /dev/disk/by-id/{by_id}"
        ).strip()
        assert device_uuid == backup_uuid, (
            f"UUID mismatch for {name}: device={device_uuid}, backup={backup_uuid}"
        )

# ============================================================================
# Phase 2: Verify pool and write test data
# ============================================================================

with subtest("pool is RAID1 with test data"):
    machine.succeed("mountpoint -q /mnt/storage")
    df_output = machine.succeed("btrfs fi df /mnt/storage")
    assert "RAID1" in df_output, f"Expected RAID1:\n{df_output}"
    machine.succeed("echo 'important data survives header corruption' > /mnt/storage/testfile.txt")
    machine.succeed("sync")

# ============================================================================
# Phase 3: Corrupt disk1 LUKS header, verify it's broken
# ============================================================================

with subtest("corrupt disk1 LUKS header"):
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup close braid-disk1")
    machine.succeed("cryptsetup close braid-disk2")
    # Overwrite LUKS header (first 4MB)
    machine.succeed("dd if=/dev/zero of=/dev/disk/by-id/virtio-disk1 bs=4096 count=1024")

with subtest("disk1 is no longer recognized as LUKS"):
    machine.fail("cryptsetup isLuks /dev/disk/by-id/virtio-disk1")

# ============================================================================
# Phase 4: Restore header from backup, verify data recovery
# ============================================================================

with subtest("restore LUKS header from backup"):
    # -q suppresses interactive "Are you sure?" confirmation
    machine.succeed(
        f"cryptsetup -q luksHeaderRestore "
        f"--header-backup-file {backup_dir}/braid-disk1.luksheader "
        "/dev/disk/by-id/virtio-disk1"
    )

with subtest("disk1 is LUKS again"):
    machine.succeed("cryptsetup isLuks /dev/disk/by-id/virtio-disk1")

with subtest("unlock with original passphrase and verify data"):
    machine.succeed(
        f"echo -n '{passphrase}' | cryptsetup luksOpen --key-file=- "
        "/dev/disk/by-id/virtio-disk1 braid-disk1"
    )
    machine.succeed(
        f"echo -n '{passphrase}' | cryptsetup luksOpen --key-file=- "
        "/dev/disk/by-id/virtio-disk2 braid-disk2"
    )
    machine.succeed("mount -o degraded /dev/mapper/braid-disk1 /mnt/storage")
    content = machine.succeed("cat /mnt/storage/testfile.txt").strip()
    assert content == "important data survives header corruption", (
        f"data mismatch: got '{content}'"
    )

# Clean up
machine.succeed("umount /mnt/storage || true")
machine.succeed("cryptsetup close braid-disk1 || true")
machine.succeed("cryptsetup close braid-disk2 || true")

machine.shutdown()
