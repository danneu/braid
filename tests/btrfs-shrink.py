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

with subtest("Write data across the pool"):
    machine.succeed("echo 'keep this' > /mnt/storage/keep.txt")
    machine.succeed("echo 'and this' > /mnt/storage/also.txt")
    machine.succeed("sync")

with subtest("Remove disk3 from live pool"):
    # btrfs migrates all data off disk3 before removing it
    machine.succeed("btrfs device remove /dev/mapper/disk3 /mnt/storage")

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print(f"After remove:\n{fi_show}")
    assert "/dev/mapper/disk3" not in fi_show, f"disk3 still in pool:\n{fi_show}"
    assert "/dev/mapper/disk1" in fi_show
    assert "/dev/mapper/disk2" in fi_show

with subtest("Data is intact after shrink"):
    content1 = machine.succeed("cat /mnt/storage/keep.txt").strip()
    content2 = machine.succeed("cat /mnt/storage/also.txt").strip()
    assert content1 == "keep this", f"Expected 'keep this', got '{content1}'"
    assert content2 == "and this", f"Expected 'and this', got '{content2}'"

with subtest("Pool still works with 2 drives"):
    machine.succeed("echo 'new write' > /mnt/storage/new.txt")
    content = machine.succeed("cat /mnt/storage/new.txt").strip()
    assert content == "new write", f"Expected 'new write', got '{content}'"

    df_output = machine.succeed("btrfs fi df /mnt/storage")
    assert "Data, RAID1" in df_output, f"Expected RAID1 profile:\n{df_output}"

machine.shutdown()
