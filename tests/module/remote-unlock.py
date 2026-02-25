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

with subtest("Unlock all 3 LUKS devices over SSH, restart cryptsetup units"):
    for name in ["disk1", "disk2", "disk3"]:
        client.succeed(
            f"{ssh_cmd}"
            f" \"echo -n testpassphrase | cryptsetup luksOpen --key-file=-"
            f" /dev/disk/by-id/virtio-{name} braid-{name}"
            f" || cryptsetup status braid-{name}\""
        )

    # Restart unit instances, not cryptsetup.target. The target does not
    # reliably retrigger member units that are still waiting for ask-password.
    for name in ["braid-disk1", "braid-disk2", "braid-disk3"]:
        escaped = name.replace("-", "\\x2d")
        unit = f"systemd-cryptsetup@{escaped}.service"
        client.execute(f"{ssh_cmd} \"systemctl restart '{unit}'\"")

with subtest("Server reaches full boot after remote unlock"):
    server.wait_for_unit("multi-user.target", timeout=120)
    server.succeed("systemctl is-active multi-user.target")

with subtest("btrfs RAID1 pool is mounted"):
    server.succeed("mountpoint /mnt/storage")

    fi_show = server.succeed("btrfs fi show /mnt/storage")
    print(f"Pool after unlock:\n{fi_show}")
    for i in range(1, 4):
        assert f"/dev/mapper/braid-disk{i}" in fi_show, (
            f"braid-disk{i} missing from pool:\n{fi_show}"
        )

    df_output = server.succeed("btrfs fi df /mnt/storage")
    assert "Data, RAID1" in df_output, f"Expected RAID1 profile:\n{df_output}"

with subtest("Pool is usable — write and read"):
    server.succeed("echo 'unlocked and ready' > /mnt/storage/test.txt")
    content = server.succeed("cat /mnt/storage/test.txt").strip()
    assert content == "unlocked and ready", f"Expected 'unlocked and ready', got '{content}'"

server.shutdown()
client.shutdown()
