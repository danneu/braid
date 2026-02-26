start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"

# --- Setup: LUKS open both disks ---

with subtest("LUKS format and open disk1"):
    dev = "/dev/disk/by-id/virtio-disk1"
    machine.succeed(f"echo -n '{passphrase}' | cryptsetup luksFormat --batch-mode --key-file=- --pbkdf pbkdf2 --pbkdf-force-iterations 1000 {dev}")
    machine.succeed(f"echo -n '{passphrase}' | cryptsetup luksOpen --key-file=- {dev} disk1")

with subtest("LUKS format and open disk2"):
    dev = "/dev/disk/by-id/virtio-disk2"
    machine.succeed(f"echo -n '{passphrase}' | cryptsetup luksFormat --batch-mode --key-file=- --pbkdf pbkdf2 --pbkdf-force-iterations 1000 {dev}")
    machine.succeed(f"echo -n '{passphrase}' | cryptsetup luksOpen --key-file=- {dev} disk2")

# --- Create single-profile btrfs and fill it ---

with subtest("Create single-drive btrfs"):
    machine.succeed("mkfs.btrfs -f /dev/mapper/disk1")
    machine.succeed("mkdir -p /mnt/storage")
    machine.succeed("mount /dev/mapper/disk1 /mnt/storage")

    df_output = machine.succeed("btrfs fi df /mnt/storage")
    assert "Data, single" in df_output, f"Expected single profile:\n{df_output}"

with subtest("Fill filesystem aggressively"):
    # dd will exit non-zero when it hits ENOSPC — that's expected
    machine.execute("dd if=/dev/zero of=/mnt/storage/fill bs=1M 2>&1 || true")
    machine.succeed("sync")

    # Log space state for debugging
    usage = machine.succeed("btrfs filesystem usage /mnt/storage")
    print(f"=== btrfs filesystem usage after fill ===\n{usage}")

# --- Add second drive and attempt balance ---

with subtest("Add disk2"):
    machine.succeed("btrfs device add -f /dev/mapper/disk2 /mnt/storage")

with subtest("Balance hits ENOSPC"):
    (exit_code, stdout) = machine.execute(
        "btrfs balance start -dconvert=raid1 -mconvert=raid1 /mnt/storage 2>&1"
    )

    print(f"=== balance exit code: {exit_code} ===")
    print(f"=== balance output ===\n{stdout}")

    # Grab dmesg for kernel-level errors
    dmesg = machine.succeed("dmesg | tail -50")
    print(f"=== dmesg tail ===\n{dmesg}")

    # Grab space state after failed balance
    usage = machine.succeed("btrfs filesystem usage /mnt/storage")
    print(f"=== btrfs filesystem usage after balance ===\n{usage}")

    assert exit_code != 0, f"Expected balance to fail, but it exited {exit_code}"

    # btrfs userspace reports "Input/output error" (EIO) — the underlying ENOSPC
    # is only visible in dmesg. Verify both: the userspace error string we'll
    # actually match in pool.rs, and the kernel-level ENOSPC confirmation.
    assert "Input/output error" in stdout, \
        f"Expected 'Input/output error' in balance output, got:\n{stdout}"
    assert "enospc" in dmesg.lower() or "No space left" in dmesg, \
        f"Expected ENOSPC in dmesg, got:\n{dmesg}"

machine.shutdown()
