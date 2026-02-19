# Phase 1 — Degraded boot (same as degraded-boot.py, without Samba)

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
    for name in ["disk1", "disk2"]:
        client.succeed(
            f"{ssh_cmd}"
            f" \"echo -n testpassphrase | cryptsetup luksOpen --key-file=-"
            f" /dev/disk/by-id/virtio-{name} {name}"
            f" || cryptsetup status {name}\""
        )

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

# Phase 2 — Replace failed drive with disk4

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def add_disk(dev):
    return (
        f"echo 'erase this disk' | "
        f"BRAID_PASSPHRASE='{passphrase}' "
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid-add-disk {dev}"
    )


with subtest("Replace dead disk3 with disk4 using braid-add-disk"):
    result = server.succeed(add_disk("/dev/disk/by-id/virtio-disk4"))
    print(f"braid-add-disk output:\n{result}")

with subtest("Pool is healthy — 3 devices, no missing"):
    fi_show = server.succeed("btrfs fi show /mnt/storage")
    print(f"Pool after replacement:\n{fi_show}")

    # Replacement drive is in the pool
    assert "virtio-disk4" in fi_show, (
        f"Replacement mapper virtio-disk4 missing from pool:\n{fi_show}"
    )

    # No more missing devices — the key assertion
    assert "missing" not in fi_show.lower(), (
        f"Pool still has missing device after replacement:\n{fi_show}"
    )

    # Exactly 3 devices
    devid_count = fi_show.count("devid")
    assert devid_count == 3, (
        f"Expected 3 devid entries, got {devid_count}:\n{fi_show}"
    )

    # Original surviving drives still present
    for name in ["disk1", "disk2"]:
        assert f"/dev/mapper/{name}" in fi_show, (
            f"{name} missing from pool after replacement:\n{fi_show}"
        )

    # Still RAID1
    df_output = server.succeed("btrfs fi df /mnt/storage")
    assert "Data, RAID1" in df_output, (
        f"Expected RAID1 profile after replacement:\n{df_output}"
    )

with subtest("Data intact after replacement"):
    content = server.succeed("cat /mnt/storage/survived.txt").strip()
    assert content == "data written before drive death", (
        f"Expected 'data written before drive death', got '{content}'"
    )

with subtest("New writes work on healthy pool"):
    server.succeed("echo 'written after replacement' > /mnt/storage/replaced.txt")
    content = server.succeed("cat /mnt/storage/replaced.txt").strip()
    assert content == "written after replacement", (
        f"Expected 'written after replacement', got '{content}'"
    )

server.shutdown()
client.shutdown()
