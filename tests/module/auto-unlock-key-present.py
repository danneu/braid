# Test: auto-unlock-key-present
#
# Intent: Verify that when autoUnlock is enabled and a USB device with a
# valid keyfile is present at boot, the pool is automatically mounted and
# the USB is unmounted after use.
#
# Why it exists: This is the primary auto-unlock use case. If systemd
# service ordering, mount unit config, or keyfile path resolution is wrong,
# users get a locked NAS after an unattended reboot.
#
# Scenario: NixOS module test. Virtual "USB" disk (serial "usbkey")
# formatted ext4 containing /braid.key. Fixture pre-creates LUKS+btrfs
# pool and enrolls keyfile. Initrd does NOT unlock LUKS — the
# braid-auto-unlock service opens LUKS and mounts the pool in stage 2.
# USB is NOT still mounted at /run/braid-key.

start_all()
machine.wait_for_unit("multi-user.target", timeout=120)

with subtest("Auto-unlock service ran successfully"):
    journal = machine.succeed(
        "journalctl -u braid-auto-unlock.service --no-pager"
    )
    assert "pool unlocked successfully" in journal, \
        f"Expected success message in journal, got:\n{journal}"

with subtest("Pool is mounted after auto-unlock"):
    machine.succeed("mountpoint /mnt/storage")

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print(f"Pool:\n{fi_show}")
    assert "/dev/mapper/braid-disk1" in fi_show, f"disk1 missing from pool:\n{fi_show}"
    assert "/dev/mapper/braid-disk2" in fi_show, f"disk2 missing from pool:\n{fi_show}"

with subtest("USB is unmounted after auto-unlock"):
    ret = machine.execute("mountpoint -q /run/braid-key")
    assert ret[0] != 0, "USB should NOT be mounted at /run/braid-key after auto-unlock"

with subtest("Mount point has correct group permissions after auto-unlock"):
    stat = machine.succeed("stat -c '%U:%G %a' /mnt/storage").strip()
    assert stat == "root:storage 2770", f"Expected root:storage 2770, got {stat}"

with subtest("Write and read round-trip"):
    machine.succeed("echo 'auto-unlock test' > /mnt/storage/test.txt")
    content = machine.succeed("cat /mnt/storage/test.txt").strip()
    assert content == "auto-unlock test", f"Expected 'auto-unlock test', got '{content}'"

machine.shutdown()
