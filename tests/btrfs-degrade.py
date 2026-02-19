start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
disks = ["disk1", "disk2", "disk3"]

with subtest("LUKS format, open, and create 3-drive btrfs RAID1"):
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

with subtest("Write data and sync"):
    machine.succeed("echo 'critical data' > /mnt/storage/important.txt")
    machine.succeed("echo 'more stuff' > /mnt/storage/other.txt")
    machine.succeed("sync")

with subtest("Simulate drive failure — kill disk3"):
    machine.succeed("umount /mnt/storage")
    # Close LUKS on disk3 — simulates the drive disappearing
    machine.succeed("cryptsetup luksClose disk3")

with subtest("Remount in degraded mode without disk3"):
    machine.succeed("mount -o degraded /dev/mapper/disk1 /mnt/storage")

with subtest("Data is readable in degraded mode"):
    content1 = machine.succeed("cat /mnt/storage/important.txt").strip()
    content2 = machine.succeed("cat /mnt/storage/other.txt").strip()
    assert content1 == "critical data", f"Expected 'critical data', got '{content1}'"
    assert content2 == "more stuff", f"Expected 'more stuff', got '{content2}'"

with subtest("New writes work in degraded mode"):
    machine.succeed("echo 'written while degraded' > /mnt/storage/degraded.txt")
    content = machine.succeed("cat /mnt/storage/degraded.txt").strip()
    assert content == "written while degraded", f"Expected 'written while degraded', got '{content}'"

with subtest("btrfs reports missing device"):
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print(f"Degraded pool:\n{fi_show}")
    assert "missing" in fi_show.lower() or "/dev/mapper/disk3" not in fi_show, \
        f"Expected missing device in output:\n{fi_show}"

machine.shutdown()
