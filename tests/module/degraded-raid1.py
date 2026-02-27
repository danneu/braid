start_all()
machine.wait_for_unit("multi-user.target", timeout=180)

with subtest("braid unlock handles bricked disk and mounts degraded"):
    machine.succeed("echo -n 'testpassphrase' | braid unlock --passphrase-stdin")
    machine.succeed("mountpoint /mnt/storage")

with subtest("Pool is mounted in degraded mode"):
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print(f"Pool:\n{fi_show}")

    # disk1 and disk2 should be present
    assert "/dev/mapper/braid-disk1" in fi_show, f"disk1 missing:\n{fi_show}"
    assert "/dev/mapper/braid-disk2" in fi_show, f"disk2 missing:\n{fi_show}"

    # disk3 should show as missing
    assert "missing" in fi_show.lower(), f"Expected 'missing' device:\n{fi_show}"

with subtest("Data written before drive death survived"):
    content = machine.succeed("cat /mnt/storage/survived.txt").strip()
    assert content == "data written before drive death", (
        f"Expected 'data written before drive death', got '{content}'"
    )

with subtest("New writes work on degraded pool"):
    machine.succeed("echo 'new data' > /mnt/storage/new.txt")
    content = machine.succeed("cat /mnt/storage/new.txt").strip()
    assert content == "new data", f"Expected 'new data', got '{content}'"

machine.shutdown()
