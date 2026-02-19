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

with subtest("Wrong passphrase is rejected and retry still works"):
    # Wrong passphrase must fail with a cryptsetup error, not a transient SSH failure.
    exit_code, stderr = client.execute(
        f"{ssh_cmd}"
        " \"echo -n wrongpassphrase | cryptsetup luksOpen --key-file=-"
        " /dev/disk/by-id/virtio-disk1 disk1 2>&1\""
    )
    assert exit_code != 0, "Wrong passphrase should have failed"
    assert "No key available" in stderr, (
        f"Expected cryptsetup rejection, got: {stderr}"
    )

    # The device must not be open after a failed attempt.
    exit_code, _ = client.execute(
        f"{ssh_cmd} 'test -e /dev/mapper/disk1'"
    )
    assert exit_code != 0, "/dev/mapper/disk1 should not exist after wrong passphrase"

    # Initrd is still waiting for passphrases — ask-password requests still present.
    client.succeed(
        f"{ssh_cmd}"
        " 'ls /run/systemd/ask-password/ask.* >/dev/null'"
    )

with subtest("Unlock all 3 LUKS devices over SSH"):
    for name in ["disk1", "disk2", "disk3"]:
        client.succeed(
            f"{ssh_cmd}"
            f" \"echo -n testpassphrase | cryptsetup luksOpen --key-file=-"
            f" /dev/disk/by-id/virtio-{name} {name}"
            f" || cryptsetup status {name}\""
        )

    # Restart cryptsetup units so systemd knows the devices are unlocked.
    # Without this, the units are still in "activating" (waiting for
    # passphrase) and downstream dependencies won't trigger.
    for name in ["disk1", "disk2", "disk3"]:
        client.succeed(
            f"{ssh_cmd}"
            f" \"systemctl restart systemd-cryptsetup@{name}.service\""
        )

with subtest("Server reaches full boot after remote unlock"):
    server.wait_for_unit("multi-user.target", timeout=120)
    server.succeed("systemctl is-active multi-user.target")

with subtest("btrfs RAID1 pool is mounted"):
    server.succeed("mountpoint /mnt/storage")

    fi_show = server.succeed("btrfs fi show /mnt/storage")
    print(f"Pool after unlock:\n{fi_show}")
    for name in ["disk1", "disk2", "disk3"]:
        assert f"/dev/mapper/{name}" in fi_show, f"{name} missing from pool:\n{fi_show}"

    df_output = server.succeed("btrfs fi df /mnt/storage")
    assert "Data, RAID1" in df_output, f"Expected RAID1 profile:\n{df_output}"

with subtest("Pool is usable — write and read"):
    server.succeed("echo 'unlocked and ready' > /mnt/storage/test.txt")
    content = server.succeed("cat /mnt/storage/test.txt").strip()
    assert content == "unlocked and ready", f"Expected 'unlocked and ready', got '{content}'"

server.shutdown()
client.shutdown()
