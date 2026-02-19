start_all()
machine.wait_for_unit("multi-user.target", timeout=120)

with subtest("btrfs RAID1 pool is mounted"):
    machine.succeed("mountpoint /mnt/storage")

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print(f"Pool:\n{fi_show}")
    for i in range(1, 4):
        assert f"/dev/mapper/virtio-disk{i}" in fi_show, (
            f"disk{i} missing from pool:\n{fi_show}"
        )

    df_output = machine.succeed("btrfs fi df /mnt/storage")
    assert "Data, RAID1" in df_output, f"Expected RAID1 profile:\n{df_output}"

with subtest("Runtime config file is generated"):
    import json
    config_raw = machine.succeed("cat /etc/braid/config.json")
    config = json.loads(config_raw)
    assert config["mountPoint"] == "/mnt/storage", f"Expected /mnt/storage, got {config['mountPoint']}"
    expected_disks = [f"/dev/disk/by-id/virtio-disk{i}" for i in range(1, 4)]
    assert config["disks"] == expected_disks, f"Unexpected disks: {config['disks']}"

with subtest("Write and read round-trip"):
    machine.succeed("echo 'raid1 data' > /mnt/storage/test.txt")
    content = machine.succeed("cat /mnt/storage/test.txt").strip()
    assert content == "raid1 data", f"Expected 'raid1 data', got '{content}'"

machine.shutdown()
