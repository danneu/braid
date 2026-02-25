start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
disks = ["disk1", "disk2", "disk3"]

with subtest("LUKS format and open all drives"):
    for name in disks:
        dev = f"/dev/disk/by-id/virtio-{name}"
        machine.succeed(f"echo -n '{passphrase}' | cryptsetup luksFormat --batch-mode --key-file=- --pbkdf pbkdf2 --pbkdf-force-iterations 1000 {dev}")
        machine.succeed(f"echo -n '{passphrase}' | cryptsetup luksOpen --key-file=- {dev} {name}")

with subtest("Create btrfs RAID1 across all three LUKS devices"):
    machine.succeed(
        "mkfs.btrfs -f -d raid1 -m raid1"
        " /dev/mapper/disk1"
        " /dev/mapper/disk2"
        " /dev/mapper/disk3"
    )

with subtest("Mount and verify RAID1 profile"):
    machine.succeed("mkdir -p /mnt/storage")
    machine.succeed("mount /dev/mapper/disk1 /mnt/storage")

    # btrfs fi df shows the allocation profile for each type
    df_output = machine.succeed("btrfs fi df /mnt/storage")
    assert "Data, RAID1" in df_output, f"Expected RAID1 data profile:\n{df_output}"
    assert "Metadata, RAID1" in df_output, f"Expected RAID1 metadata profile:\n{df_output}"

with subtest("All three devices visible in the filesystem"):
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    for name in disks:
        assert f"/dev/mapper/{name}" in fi_show, f"{name} not in btrfs fi show:\n{fi_show}"

with subtest("Write and read back a file"):
    machine.succeed("echo 'hello braid' > /mnt/storage/test.txt")
    content = machine.succeed("cat /mnt/storage/test.txt").strip()
    assert content == "hello braid", f"Expected 'hello braid', got '{content}'"

machine.shutdown()
