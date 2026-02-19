start_all()
machine.wait_for_unit("multi-user.target", timeout=120)

with subtest("btrfs single-disk pool is mounted"):
    machine.succeed("mountpoint /mnt/storage")

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print(f"Pool:\n{fi_show}")
    assert "/dev/mapper/virtio-disk1" in fi_show, f"disk missing from pool:\n{fi_show}"

    df_output = machine.succeed("btrfs fi df /mnt/storage")
    assert "Data, single" in df_output, f"Expected single profile:\n{df_output}"

with subtest("Write and read round-trip"):
    machine.succeed("echo 'hello btrnas' > /mnt/storage/test.txt")
    content = machine.succeed("cat /mnt/storage/test.txt").strip()
    assert content == "hello btrnas", f"Expected 'hello btrnas', got '{content}'"

machine.shutdown()
