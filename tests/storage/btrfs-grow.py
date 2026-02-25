start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"

# --- Phase 1: Start with 2-drive RAID1 ---

with subtest("LUKS format and open disk1 + disk2"):
    for name in ["disk1", "disk2"]:
        dev = f"/dev/disk/by-id/virtio-{name}"
        machine.succeed(f"echo -n '{passphrase}' | cryptsetup luksFormat --batch-mode --key-file=- --pbkdf pbkdf2 --pbkdf-force-iterations 1000 {dev}")
        machine.succeed(f"echo -n '{passphrase}' | cryptsetup luksOpen --key-file=- {dev} {name}")

with subtest("Create btrfs RAID1 on 2 drives"):
    machine.succeed(
        "mkfs.btrfs -f -d raid1 -m raid1"
        " /dev/mapper/disk1"
        " /dev/mapper/disk2"
    )
    machine.succeed("mkdir -p /mnt/storage")
    machine.succeed("mount /dev/mapper/disk1 /mnt/storage")

with subtest("Write data and record pool size"):
    machine.succeed("echo 'before expansion' > /mnt/storage/original.txt")
    machine.succeed("sync")

    fi_show_before = machine.succeed("btrfs fi show /mnt/storage")
    print(f"Before expansion:\n{fi_show_before}")
    assert "/dev/mapper/disk1" in fi_show_before
    assert "/dev/mapper/disk2" in fi_show_before
    assert "/dev/mapper/disk3" not in fi_show_before

# --- Phase 2: Add disk3 to the live pool ---

with subtest("LUKS format and open disk3"):
    dev = "/dev/disk/by-id/virtio-disk3"
    machine.succeed(f"echo -n '{passphrase}' | cryptsetup luksFormat --batch-mode --key-file=- --pbkdf pbkdf2 --pbkdf-force-iterations 1000 {dev}")
    machine.succeed(f"echo -n '{passphrase}' | cryptsetup luksOpen --key-file=- {dev} disk3")

with subtest("Add disk3 to live btrfs pool"):
    machine.succeed("btrfs device add /dev/mapper/disk3 /mnt/storage")

    fi_show_after = machine.succeed("btrfs fi show /mnt/storage")
    print(f"After add:\n{fi_show_after}")
    assert "/dev/mapper/disk3" in fi_show_after

with subtest("Balance to distribute data across all 3 drives"):
    machine.succeed("btrfs balance start -dconvert=raid1 -mconvert=raid1 /mnt/storage")

# --- Phase 3: Verify ---

with subtest("Original data is intact"):
    content = machine.succeed("cat /mnt/storage/original.txt").strip()
    assert content == "before expansion", f"Expected 'before expansion', got '{content}'"

with subtest("New writes work on expanded pool"):
    machine.succeed("echo 'after expansion' > /mnt/storage/new.txt")
    content = machine.succeed("cat /mnt/storage/new.txt").strip()
    assert content == "after expansion", f"Expected 'after expansion', got '{content}'"

with subtest("All 3 devices active in pool"):
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    for name in ["disk1", "disk2", "disk3"]:
        assert f"/dev/mapper/{name}" in fi_show, f"{name} missing from pool:\n{fi_show}"

machine.shutdown()
