start_all()
machine.wait_for_unit("multi-user.target", timeout=120)

with subtest("braid unlock opens LUKS and mounts pool"):
    machine.succeed("echo -n 'testpassphrase' | braid unlock --passphrase-stdin")
    machine.succeed("mountpoint /mnt/storage")

with subtest("btrfs RAID1 pool has all 3 disks"):
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print(f"Pool:\n{fi_show}")
    for i in range(1, 4):
        assert f"/dev/mapper/braid-disk{i}" in fi_show, (
            f"disk{i} missing from pool:\n{fi_show}"
        )

    df_output = machine.succeed("btrfs fi df /mnt/storage")
    assert "Data, RAID1" in df_output, f"Expected RAID1 profile:\n{df_output}"

with subtest("Runtime config file is generated"):
    import json
    config_raw = machine.succeed("cat /etc/braid/config.json")
    config = json.loads(config_raw)
    assert config["mount_point"] == "/mnt/storage", f"Expected /mnt/storage, got {config['mount_point']}"
    for i in range(1, 4):
        name = f"disk{i}"
        assert name in config["disks"], f"Expected {name} in disks: {config['disks']}"
        assert config["disks"][name]["by_id"] == f"/dev/disk/by-id/virtio-disk{i}", f"Unexpected by_id for {name}: {config['disks']}"

with subtest("Write and read round-trip"):
    machine.succeed("echo 'raid1 data' > /mnt/storage/test.txt")
    content = machine.succeed("cat /mnt/storage/test.txt").strip()
    assert content == "raid1 data", f"Expected 'raid1 data', got '{content}'"

machine.shutdown()
