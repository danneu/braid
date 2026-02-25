# Test: replace-failed-disk (Phase 2 — replace dead disk with intent CLI)
#
# What: After a degraded boot (Phase 1), the dead disk3 is replaced with a
# fresh disk4 using `braid replace --old disk3 --new disk4 --yes`. The pool
# returns to healthy 3-drive RAID1 with all data intact.
#
# Why: This is the scariest real-world scenario — a drive dies, you boot
# degraded, and you need to replace it without reinstalling. It crosses every
# integration boundary: initrd SSH, degraded btrfs, and the intent CLI. No
# other test covers this full recovery cycle.
#
# Dependencies: degraded-boot (initrd SSH + degraded mount), braid add/replace.

# Phase 1 — Degraded boot (initrd SSH unlock with braid-* mapper names)

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

with subtest("Unlock disk1 and disk2 over SSH, restart cryptsetup units"):
    # Unlock the two healthy drives (disk3 is bricked).
    # Mapper names are braid-disk1, braid-disk2 per the NixOS LUKS config.
    for name in ["disk1", "disk2"]:
        client.succeed(
            f"{ssh_cmd}"
            f" \"echo -n testpassphrase | cryptsetup luksOpen --key-file=-"
            f" /dev/disk/by-id/virtio-{name} braid-{name}"
            f" || cryptsetup status braid-{name}\""
        )

    # Important: restart unit instances, not cryptsetup.target.
    # The target does not retrigger member units already stuck in "activating"
    # (waiting for ask-password). btrfs-device-scan has After= on all three
    # cryptsetup units, so boot blocks until each is terminal (active/failed).
    # For the bricked drive, restart forces a fast "failed" state; for healthy
    # drives, restart makes them "active", unblocking initrd-fs.target.
    for name in ["braid-disk1", "braid-disk2", "braid-disk3"]:
        escaped = name.replace("-", "\\x2d")
        unit = f"systemd-cryptsetup@{escaped}.service"
        client.execute(f"{ssh_cmd} \"systemctl restart '{unit}'\"")

with subtest("Server reaches full boot after degraded unlock"):
    server.wait_for_unit("multi-user.target", timeout=120)
    server.succeed("systemctl is-active multi-user.target")

with subtest("btrfs mounted in degraded mode — disk1+disk2 present, disk3 missing"):
    server.succeed("mountpoint /mnt/storage")

    fi_show = server.succeed("btrfs fi show /mnt/storage")
    print(f"Pool after degraded boot:\n{fi_show}")
    for name in ["braid-disk1", "braid-disk2"]:
        assert f"/dev/mapper/{name}" in fi_show, f"{name} missing from pool:\n{fi_show}"
    assert "missing" in fi_show.lower(), f"Expected 'missing' device in pool:\n{fi_show}"

    df_output = server.succeed("btrfs fi df /mnt/storage")
    assert "Data, RAID1" in df_output, f"Expected RAID1 profile:\n{df_output}"

with subtest("Pre-existing data survived drive death"):
    content = server.succeed("cat /mnt/storage/survived.txt").strip()
    assert content == "data written before drive death", (
        f"Expected 'data written before drive death', got '{content}'"
    )

# Phase 2 — Replace dead disk3 with disk4 using `braid replace`

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


import shlex


def replace_cmd(old, new):
    """Build a `braid replace --old <old> --new <new> --yes` command."""
    passphrase_q = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {passphrase_q} | "
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid replace --old {old} --new {new} --passphrase-stdin --yes"
    )


with subtest("Replace dead disk3 with disk4"):
    result = server.succeed(replace_cmd("disk3", "disk4"))
    print(f"braid replace output:\n{result}")

with subtest("Pool is healthy — 3 devices, no missing"):
    fi_show = server.succeed("btrfs fi show /mnt/storage")
    print(f"Pool after replacement:\n{fi_show}")

    # Replacement drive is in the pool (mapper = braid-disk4)
    assert "/dev/mapper/braid-disk4" in fi_show, (
        f"Replacement mapper braid-disk4 missing from pool:\n{fi_show}"
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
    for name in ["braid-disk1", "braid-disk2"]:
        assert f"/dev/mapper/{name}" in fi_show, (
            f"{name} missing from pool after replacement:\n{fi_show}"
        )

    # Still RAID1
    df_output = server.succeed("btrfs fi df /mnt/storage")
    assert "Data, RAID1" in df_output, (
        f"Expected RAID1 profile after replacement:\n{df_output}"
    )

with subtest("Dead disk3 mapper is NOT in pool"):
    fi_show = server.succeed("btrfs fi show /mnt/storage")
    assert "braid-disk3" not in fi_show, (
        f"Dead braid-disk3 should not be in pool:\n{fi_show}"
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
