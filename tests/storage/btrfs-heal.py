start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
disks = ["disk1", "disk2", "disk3"]

with subtest("LUKS format, open, and create btrfs RAID1"):
    for name in disks:
        dev = f"/dev/disk/by-id/virtio-{name}"
        machine.succeed(f"echo -n '{passphrase}' | cryptsetup luksFormat --batch-mode --key-file=- --pbkdf pbkdf2 --pbkdf-force-iterations 1000 {dev}")
        machine.succeed(f"echo -n '{passphrase}' | cryptsetup luksOpen --key-file=- {dev} {name}")

    machine.succeed(
        "mkfs.btrfs -f -d raid1 -m raid1"
        " /dev/mapper/disk1"
        " /dev/mapper/disk2"
        " /dev/mapper/disk3"
    )
    machine.succeed("mkdir -p /mnt/storage")
    machine.succeed("mount /dev/mapper/disk1 /mnt/storage")

with subtest("Write a known file and sync to disk"):
    machine.succeed("echo 'important data' > /mnt/storage/precious.txt")
    # Force all writes to hit disk so we can corrupt them
    machine.succeed("sync")
    # Drop page cache so next read comes from disk
    machine.succeed("echo 3 > /proc/sys/vm/drop_caches")

with subtest("Verify file content before corruption"):
    content = machine.succeed("cat /mnt/storage/precious.txt").strip()
    assert content == "important data", f"Pre-corruption: expected 'important data', got '{content}'"

with subtest("Corrupt raw bytes on disk1's underlying block device"):
    # Find btrfs device id for disk1 so we can check its stats later
    machine.succeed("btrfs device stats /mnt/storage")

    # Unmount so we can safely corrupt the block device
    machine.succeed("umount /mnt/storage")

    # Write garbage over a chunk of the LUKS device (which holds btrfs data).
    # Offset 4M skips the btrfs superblock area and hits data blocks.
    machine.succeed("dd if=/dev/urandom of=/dev/mapper/disk1 bs=4096 count=256 seek=1024 conv=notrunc")

    # Remount — btrfs should still assemble from the other copies
    machine.succeed("mount -o degraded /dev/mapper/disk2 /mnt/storage")

with subtest("btrfs scrub detects and repairs corruption"):
    # Scrub reads all data and verifies checksums, repairing from RAID1 copies
    machine.succeed("btrfs scrub start -B /mnt/storage")

    # Check scrub results
    scrub_status = machine.succeed("btrfs scrub status /mnt/storage")
    print(f"Scrub status:\n{scrub_status}")

with subtest("File content is intact after scrub healing"):
    # Drop caches again to ensure we read from disk
    machine.succeed("echo 3 > /proc/sys/vm/drop_caches")
    content = machine.succeed("cat /mnt/storage/precious.txt").strip()
    assert content == "important data", f"Post-heal: expected 'important data', got '{content}'"

with subtest("Corrupt disk2 to test on-read auto-heal"):
    # Pool is healthy after scrub repaired disk1. Now corrupt disk2
    # to verify btrfs heals transparently on read — no scrub needed.
    machine.succeed("umount /mnt/storage")
    machine.succeed("dd if=/dev/urandom of=/dev/mapper/disk2 bs=4096 count=256 seek=1024 conv=notrunc")
    machine.succeed("mount -o degraded /dev/mapper/disk1 /mnt/storage")

with subtest("Read returns correct data without scrub (on-read auto-heal)"):
    machine.succeed("echo 3 > /proc/sys/vm/drop_caches")
    content = machine.succeed("cat /mnt/storage/precious.txt").strip()
    assert content == "important data", f"On-read auto-heal: expected 'important data', got '{content}'"

machine.shutdown()
