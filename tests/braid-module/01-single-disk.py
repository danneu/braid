start_all()
machine.wait_for_unit("multi-user.target", timeout=120)

with subtest("btrfs single-disk pool is mounted"):
    machine.succeed("mountpoint /mnt/storage")

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print(f"Pool:\n{fi_show}")
    assert "/dev/mapper/virtio-disk1" in fi_show, f"disk missing from pool:\n{fi_show}"

    df_output = machine.succeed("btrfs fi df /mnt/storage")
    assert "Data, single" in df_output, f"Expected single profile:\n{df_output}"

with subtest("Runtime config file is generated"):
    import json
    config_raw = machine.succeed("cat /etc/braid/config.json")
    config = json.loads(config_raw)
    assert config["mountPoint"] == "/mnt/storage", f"Expected /mnt/storage, got {config['mountPoint']}"
    assert config["disks"] == ["/dev/disk/by-id/virtio-disk1"], f"Unexpected disks: {config['disks']}"

with subtest("CLI tools are on PATH"):
    machine.succeed("which braid-add-disk")
    machine.succeed("which braid-remove-disk")
    machine.succeed("which braid-status")

with subtest("Write and read round-trip"):
    machine.succeed("echo 'hello braid' > /mnt/storage/test.txt")
    content = machine.succeed("cat /mnt/storage/test.txt").strip()
    assert content == "hello braid", f"Expected 'hello braid', got '{content}'"

machine.shutdown()
