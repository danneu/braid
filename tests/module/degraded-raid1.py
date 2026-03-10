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

with subtest("braid unlock handles bricked disk and mounts degraded"):
    machine.succeed("echo -n 'testpassphrase' | braid unlock --passphrase-stdin --allow-degraded")
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
