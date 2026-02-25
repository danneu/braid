smb_password = "smbTestPass123"

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

with subtest("Unlock LUKS over SSH"):
    client.succeed(
        f"{ssh_cmd}"
        " \"echo -n testpassphrase | cryptsetup luksOpen --key-file=-"
        " /dev/disk/by-id/virtio-disk1 disk1"
        " || cryptsetup status disk1\""
    )
    client.succeed(
        f"{ssh_cmd}"
        " \"systemctl restart systemd-cryptsetup@disk1.service\""
    )

with subtest("Server reaches full boot"):
    server.wait_for_unit("multi-user.target", timeout=120)

with subtest("btrfs single-disk pool is mounted"):
    server.succeed("mountpoint /mnt/storage")

    fi_show = server.succeed("btrfs fi show /mnt/storage")
    print(f"Pool after unlock:\n{fi_show}")
    assert "/dev/mapper/disk1" in fi_show, f"disk1 missing from pool:\n{fi_show}"

    df_output = server.succeed("btrfs fi df /mnt/storage")
    assert "Data, single" in df_output, f"Expected single profile:\n{df_output}"

with subtest("Seed a file on the pool"):
    server.succeed("chown nas /mnt/storage")
    server.succeed("echo 'day one' > /mnt/storage/hello.txt")
    server.succeed("chown nas /mnt/storage/hello.txt")

with subtest("Samba serves the share"):
    server.succeed(
        f"(echo '{smb_password}'; echo '{smb_password}') | smbpasswd -a -s nas"
    )
    server.succeed("systemctl restart samba-smbd")
    server.wait_for_unit("samba-smbd")
    server.succeed("smbclient -L localhost -N | grep -i storage")

with subtest("Client mounts SMB and reads the seed file"):
    client.succeed("mkdir -p /mnt/nas")
    client.succeed(
        f"mount -t cifs //server/storage /mnt/nas"
        f" -o username=nas,password={smb_password},vers=3.0"
    )
    client.succeed("mountpoint /mnt/nas")

    content = client.succeed("cat /mnt/nas/hello.txt").strip()
    assert content == "day one", f"Expected 'day one', got '{content}'"

with subtest("Client writes, server reads back"):
    client.succeed("echo 'from macbook' > /mnt/nas/photos.txt")
    content = server.succeed("cat /mnt/storage/photos.txt").strip()
    assert content == "from macbook", f"Expected 'from macbook', got '{content}'"

server.shutdown()
client.shutdown()
