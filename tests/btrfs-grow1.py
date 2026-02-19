start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"

# --- Phase 1: Single drive, no redundancy ---

with subtest("LUKS format and open disk1"):
    dev = "/dev/disk/by-id/virtio-disk1"
    machine.succeed(f"echo -n '{passphrase}' | cryptsetup luksFormat --batch-mode --key-file=- --pbkdf pbkdf2 --pbkdf-force-iterations 1000 {dev}")
    machine.succeed(f"echo -n '{passphrase}' | cryptsetup luksOpen --key-file=- {dev} disk1")

with subtest("Create single-drive btrfs"):
    machine.succeed("mkfs.btrfs -f /dev/mapper/disk1")
    machine.succeed("mkdir -p /mnt/storage")
    machine.succeed("mount /dev/mapper/disk1 /mnt/storage")

    df_output = machine.succeed("btrfs fi df /mnt/storage")
    assert "Data, single" in df_output, f"Expected single profile:\n{df_output}"

with subtest("Write data on single drive"):
    machine.succeed("echo 'day one data' > /mnt/storage/day1.txt")
    machine.succeed("sync")

# --- Phase 2: Add disk2, convert to RAID1 ---

with subtest("LUKS format and open disk2"):
    dev = "/dev/disk/by-id/virtio-disk2"
    machine.succeed(f"echo -n '{passphrase}' | cryptsetup luksFormat --batch-mode --key-file=- --pbkdf pbkdf2 --pbkdf-force-iterations 1000 {dev}")
    machine.succeed(f"echo -n '{passphrase}' | cryptsetup luksOpen --key-file=- {dev} disk2")

with subtest("Add disk2 and convert to RAID1"):
    machine.succeed("btrfs device add /dev/mapper/disk2 /mnt/storage")
    machine.succeed("btrfs balance start -dconvert=raid1 -mconvert=raid1 /mnt/storage")

    df_output = machine.succeed("btrfs fi df /mnt/storage")
    assert "Data, RAID1" in df_output, f"Expected RAID1 after conversion:\n{df_output}"

with subtest("Day 1 data survived conversion"):
    content = machine.succeed("cat /mnt/storage/day1.txt").strip()
    assert content == "day one data", f"Expected 'day one data', got '{content}'"

with subtest("Write more data on 2-drive RAID1"):
    machine.succeed("echo 'day two data' > /mnt/storage/day2.txt")
    machine.succeed("sync")

# --- Phase 3: Add disk3 ---

with subtest("LUKS format and open disk3"):
    dev = "/dev/disk/by-id/virtio-disk3"
    machine.succeed(f"echo -n '{passphrase}' | cryptsetup luksFormat --batch-mode --key-file=- --pbkdf pbkdf2 --pbkdf-force-iterations 1000 {dev}")
    machine.succeed(f"echo -n '{passphrase}' | cryptsetup luksOpen --key-file=- {dev} disk3")

with subtest("Add disk3 and rebalance"):
    machine.succeed("btrfs device add /dev/mapper/disk3 /mnt/storage")
    machine.succeed("btrfs balance start -dconvert=raid1 -mconvert=raid1 /mnt/storage")

# --- Verify ---

with subtest("All data survived"):
    content1 = machine.succeed("cat /mnt/storage/day1.txt").strip()
    content2 = machine.succeed("cat /mnt/storage/day2.txt").strip()
    assert content1 == "day one data", f"Expected 'day one data', got '{content1}'"
    assert content2 == "day two data", f"Expected 'day two data', got '{content2}'"

with subtest("All 3 devices in pool with RAID1 profile"):
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    for name in ["disk1", "disk2", "disk3"]:
        assert f"/dev/mapper/{name}" in fi_show, f"{name} missing:\n{fi_show}"

    df_output = machine.succeed("btrfs fi df /mnt/storage")
    assert "Data, RAID1" in df_output, f"Expected RAID1:\n{df_output}"

with subtest("New writes work on 3-drive pool"):
    machine.succeed("echo 'day three data' > /mnt/storage/day3.txt")
    content = machine.succeed("cat /mnt/storage/day3.txt").strip()
    assert content == "day three data", f"Expected 'day three data', got '{content}'"

machine.shutdown()
