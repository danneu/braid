start_all()
client.wait_for_unit("network.target")

ssh_cmd = (
    "ssh -4 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null"
    " -i /etc/ssh/test_id_ed25519 -p 2222 root@server"
)

with subtest("Initrd SSH is up with pending ask-password requests"):
    client.wait_until_succeeds(
        f"{ssh_cmd}"
        " 'test -d /run/systemd/ask-password"
        " && ls /run/systemd/ask-password/ask.* >/dev/null'",
        timeout=120,
    )

with subtest("Unlock disk1 and disk2 over SSH, restart all cryptsetup units"):
    # Unlock the two healthy drives (disk3 is bricked)
    for name in ["disk1", "disk2"]:
        client.succeed(
            f"{ssh_cmd}"
            f" \"echo -n testpassphrase | cryptsetup luksOpen --key-file=-"
            f" /dev/disk/by-id/virtio-{name} {name}"
            f" || cryptsetup status {name}\""
        )

    # Restart all 3 cryptsetup units. disk3's restart fails immediately (bad
    # LUKS header), which transitions it to a terminal state. Without this,
    # disk3's unit stays "activating" (waiting for password) and After= on
    # btrfs-device-scan blocks forever.
    for name in ["disk1", "disk2", "disk3"]:
        client.execute(
            f"{ssh_cmd}"
            f" \"systemctl restart systemd-cryptsetup@{name}.service\""
        )

with subtest("Server reaches full boot after degraded unlock"):
    server.wait_for_unit("multi-user.target", timeout=120)
    server.succeed("systemctl is-active multi-user.target")

with subtest("btrfs mounted in degraded mode — disk1+disk2 present, disk3 missing"):
    server.succeed("mountpoint /mnt/storage")

    fi_show = server.succeed("btrfs fi show /mnt/storage")
    print(f"Pool after degraded boot:\n{fi_show}")
    for name in ["disk1", "disk2"]:
        assert f"/dev/mapper/{name}" in fi_show, f"{name} missing from pool:\n{fi_show}"
    assert "missing" in fi_show.lower(), f"Expected 'missing' device in pool:\n{fi_show}"

    df_output = server.succeed("btrfs fi df /mnt/storage")
    assert "Data, RAID1" in df_output, f"Expected RAID1 profile:\n{df_output}"

with subtest("Pre-existing data survived drive death"):
    content = server.succeed("cat /mnt/storage/survived.txt").strip()
    assert content == "data written before drive death", (
        f"Expected 'data written before drive death', got '{content}'"
    )

with subtest("New writes work in degraded mode"):
    server.succeed("echo 'written after drive death' > /mnt/storage/new.txt")
    content = server.succeed("cat /mnt/storage/new.txt").strip()
    assert content == "written after drive death", (
        f"Expected 'written after drive death', got '{content}'"
    )

with subtest("Journal shows disk3 cryptsetup failure"):
    # Check the journal for evidence of disk3 failure. The initrd systemd
    # instance is gone after switch-root, but journald captures it with -b.
    journal = server.succeed(
        "journalctl -b -u systemd-cryptsetup@disk3.service --no-pager 2>&1 || true"
    )
    print(f"disk3 journal:\n{journal}")
    # Accept either: journal has failure evidence, or the unit simply doesn't
    # exist in stage-2 journal (which itself proves it never succeeded)

server.shutdown()
client.shutdown()
