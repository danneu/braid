start_all()
machine.wait_for_unit("multi-user.target", timeout=120)

with subtest("braid unlock opens LUKS and mounts pool"):
    machine.succeed("echo -n 'testpassphrase' | braid unlock --passphrase-stdin")
    machine.succeed("mountpoint /mnt/storage")

with subtest("btrfs single-disk pool has correct profile"):
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print(f"Pool:\n{fi_show}")
    assert "/dev/mapper/braid-disk1" in fi_show, f"disk missing from pool:\n{fi_show}"

    df_output = machine.succeed("btrfs fi df /mnt/storage")
    assert "Data, single" in df_output, f"Expected single profile:\n{df_output}"

with subtest("Runtime config file is generated"):
    import json
    config_raw = machine.succeed("cat /etc/braid/config.json")
    config = json.loads(config_raw)
    assert config["mount_point"] == "/mnt/storage", f"Expected /mnt/storage, got {config['mount_point']}"
    assert "disk1" in config["disks"], f"Expected disk1 in disks: {config['disks']}"
    assert config["disks"]["disk1"]["by_id"] == "/dev/disk/by-id/virtio-disk1", f"Unexpected by_id: {config['disks']}"

with subtest("Unified CLI is on PATH"):
    machine.succeed("which braid")

with subtest("Write and read round-trip"):
    machine.succeed("echo 'hello braid' > /mnt/storage/test.txt")
    content = machine.succeed("cat /mnt/storage/test.txt").strip()
    assert content == "hello braid", f"Expected 'hello braid', got '{content}'"

machine.shutdown()
