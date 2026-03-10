import json

start_all()
machine.wait_for_unit("multi-user.target", timeout=180)

# Seed disk-map: the initrd fixture bypasses `braid add`, so the disk-map is
# empty. braid unlock uses the disk-map to distinguish bricked pool members
# (degradable) from uninitialized disks (hard error).
disk_map = json.dumps({"disks": {
    d: {"by_id": f"/dev/disk/by-id/virtio-{d}", "luks_uuid": "x", "devid": i, "added_at": "t"}
    for i, d in enumerate(["disk1", "disk2", "disk3"], 1)
}})
machine.succeed(f"mkdir -p /var/lib/braid && echo '{disk_map}' > /var/lib/braid/disk-map.json")

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

with subtest("braid unlock refuses degraded mount without --allow-degraded"):
    ret = machine.execute(
        "echo -n 'testpassphrase' | braid unlock --passphrase-stdin 2>&1"
    )
    assert ret[0] != 0, "Expected refusal"
    assert "refusing to mount degraded" in ret[1], \
        f"Expected 'refusing to mount degraded' in output, got: {ret[1]}"
    machine.fail("mountpoint -q /mnt/storage")

with subtest("braid unlock --allow-degraded mounts degraded"):
    machine.succeed(
        "echo -n 'testpassphrase' | braid unlock --passphrase-stdin --allow-degraded"
    )
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
