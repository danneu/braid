start_all()
server.wait_for_unit("multi-user.target")
client.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
smb_password = "smbTestPass123"
disks = ["disk1", "disk2", "disk3"]

with subtest("LUKS format, open, and create btrfs RAID1 on server"):
    for name in disks:
        dev = f"/dev/disk/by-id/virtio-{name}"
        server.succeed(f"echo -n '{passphrase}' | cryptsetup luksFormat --batch-mode --key-file=- --pbkdf pbkdf2 --pbkdf-force-iterations 1000 {dev}")
        server.succeed(f"echo -n '{passphrase}' | cryptsetup luksOpen --key-file=- {dev} {name}")

    server.succeed(
        "mkfs.btrfs -f -d raid1 -m raid1"
        " /dev/mapper/disk1"
        " /dev/mapper/disk2"
        " /dev/mapper/disk3"
    )
    server.succeed("mkdir -p /mnt/storage")
    server.succeed("mount /dev/mapper/disk1 /mnt/storage")
    server.succeed("chown root:storage /mnt/storage")
    server.succeed("chmod 2770 /mnt/storage")

with subtest("Set up Samba password and restart"):
    server.succeed(f"(echo '{smb_password}'; echo '{smb_password}') | smbpasswd -a -s nas")
    server.succeed("systemctl restart samba-smbd")
    server.wait_for_unit("samba-smbd")

with subtest("Server share is visible"):
    server.succeed("smbclient -L localhost -N | grep -i storage")

with subtest("Client mounts SMB share"):
    client.succeed("mkdir -p /mnt/nas")
    client.succeed(f"mount -t cifs //server/storage /mnt/nas -o username=nas,password={smb_password},vers=3.0")
    client.succeed("mountpoint /mnt/nas")

with subtest("Client write/read round-trip"):
    client.succeed("echo 'from the client' > /mnt/nas/hello.txt")
    content = client.succeed("cat /mnt/nas/hello.txt").strip()
    assert content == "from the client", f"Expected 'from the client', got '{content}'"

with subtest("File landed on btrfs pool on server"):
    content = server.succeed("cat /mnt/storage/hello.txt").strip()
    assert content == "from the client", f"Server expected 'from the client', got '{content}'"

server.shutdown()
client.shutdown()
