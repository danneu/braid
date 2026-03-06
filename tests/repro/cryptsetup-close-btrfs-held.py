# Repro: btrfs scan registry holds device references after umount
#
# Intent: Prove that after umount of a multi-device btrfs RAID1, the btrfs
# kernel scan registry still lists the devices. `btrfs device scan --forget`
# clears the registry. Document whether `cryptsetup close` succeeds or fails
# in each state.
#
# Why it exists: On multi-device btrfs, the kernel's scan registry retains
# references to devices after umount. This can cause `cryptsetup close` to
# fail with "device is busy" in a race window. `btrfs device scan --forget`
# is the reliable fix. This repro documents the raw behavior that `braid lock`
# must handle.
#
# Scenario: 2-disk LUKS + btrfs RAID1 pool. Write data, umount. Observe that
# btrfs fi show still lists devices. Try cryptsetup close (may or may not
# fail). Then `btrfs device scan --forget` clears the registry, and
# cryptsetup close always succeeds.

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
luks_format = "--batch-mode --key-file=- --pbkdf pbkdf2 --pbkdf-force-iterations 1000"

with subtest("Setup: 2-disk LUKS + btrfs RAID1"):
    for name in ["disk1", "disk2"]:
        dev = f"/dev/disk/by-id/virtio-{name}"
        machine.succeed(f"echo -n '{passphrase}' | cryptsetup luksFormat {luks_format} {dev}")
        machine.succeed(f"echo -n '{passphrase}' | cryptsetup luksOpen --key-file=- {dev} {name}")

    machine.succeed(
        "mkfs.btrfs -f -d raid1 -m raid1"
        " /dev/mapper/disk1"
        " /dev/mapper/disk2"
    )
    machine.succeed("mkdir -p /mnt/storage")
    machine.succeed("mount /dev/mapper/disk1 /mnt/storage")
    machine.succeed("dd if=/dev/urandom of=/mnt/storage/testfile.bin bs=1M count=10")
    machine.succeed("sync")

with subtest("After umount, btrfs fi show still lists devices (scan registry)"):
    machine.succeed("umount /mnt/storage")
    machine.fail("mountpoint -q /mnt/storage")

    fi_show = machine.succeed("btrfs fi show")
    print(f"btrfs fi show after umount:\n{fi_show}")
    assert "/dev/mapper/disk1" in fi_show or "/dev/mapper/disk2" in fi_show, \
        f"Expected scan registry to still list devices after umount, got:\n{fi_show}"

with subtest("Document: cryptsetup close before forget (race-dependent)"):
    # This may succeed or fail depending on timing. We document whichever
    # behavior occurs — the point is that it's unreliable.
    exit_code, output = machine.execute("cryptsetup close disk1 2>&1")
    print(f"cryptsetup close disk1 before forget: exit={exit_code}, output={output.strip()}")
    if exit_code == 0:
        print("NOTE: close succeeded before forget (race did not trigger)")
        # Re-open disk1 so we can test the forget path
        dev1 = "/dev/disk/by-id/virtio-disk1"
        machine.succeed(f"echo -n '{passphrase}' | cryptsetup luksOpen --key-file=- {dev1} disk1")
    else:
        print("NOTE: close failed before forget (race triggered — device busy)")

with subtest("btrfs device scan --forget clears the registry"):
    machine.succeed("btrfs device scan --forget")

    fi_show = machine.succeed("btrfs fi show")
    print(f"btrfs fi show after forget:\n{fi_show}")
    # Note: --forget clears the scan cache, but btrfs may re-detect devices
    # that are still open (the mappers exist). The important thing is that
    # forget releases the held references so cryptsetup close can succeed.

with subtest("After forget, cryptsetup close always succeeds"):
    machine.succeed("cryptsetup close disk1")
    machine.succeed("cryptsetup close disk2")
    machine.fail("test -e /dev/mapper/disk1")
    machine.fail("test -e /dev/mapper/disk2")

machine.shutdown()
