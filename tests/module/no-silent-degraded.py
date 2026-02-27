start_all()
machine.wait_for_unit("multi-user.target", timeout=180)

with subtest("Pool is not mounted after boot (LUKS closed, no degraded in mount unit)"):
    machine.fail("mountpoint -q /mnt/storage")

with subtest("Open LUKS on surviving disks and scan for btrfs members"):
    # Simulate the state where LUKS devices are open but braid-unlock hasn't
    # run yet (e.g. auto-unlock opened them, or user opened manually).
    machine.succeed(
        "echo -n 'testpassphrase' | cryptsetup luksOpen --key-file=- "
        "/dev/disk/by-id/virtio-disk1 braid-disk1"
    )
    machine.succeed(
        "echo -n 'testpassphrase' | cryptsetup luksOpen --key-file=- "
        "/dev/disk/by-id/virtio-disk2 braid-disk2"
    )
    # disk3 is bricked — cannot open

    # Let the kernel discover the (incomplete) btrfs pool
    machine.succeed("btrfs device scan")

with subtest("Direct mount without -o degraded refuses to mount with missing device"):
    # This is the critical assertion: without 'degraded', btrfs refuses to
    # mount a RAID1 pool that has a missing member device. This proves that
    # removing 'degraded' from fstab/fileSystems prevents silent degraded mounts.
    machine.fail("mount /dev/mapper/braid-disk1 /mnt/storage")
    machine.fail("mountpoint -q /mnt/storage")

with subtest("braid unlock detects missing disk and mounts degraded"):
    # braid unlock sees disk1+disk2 already open, disk3 as PresentNotLuks
    # (bricked header). It adds -o degraded dynamically.
    machine.succeed("echo -n 'testpassphrase' | braid unlock --passphrase-stdin")
    machine.succeed("mountpoint -q /mnt/storage")

with subtest("Pool shows degraded state with missing device"):
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print(f"Pool:\n{fi_show}")

    assert "/dev/mapper/braid-disk1" in fi_show, f"disk1 missing:\n{fi_show}"
    assert "/dev/mapper/braid-disk2" in fi_show, f"disk2 missing:\n{fi_show}"
    assert "missing" in fi_show.lower(), f"Expected 'missing' device:\n{fi_show}"

with subtest("Data written before drive death survived"):
    content = machine.succeed("cat /mnt/storage/survived.txt").strip()
    assert content == "data written before drive death", (
        f"Expected 'data written before drive death', got '{content}'"
    )

machine.shutdown()
