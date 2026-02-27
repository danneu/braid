# Repro: btrfs replace start rejects a smaller target device
#
# Intent: Confirm that `btrfs replace start` fails when the target device is
# smaller than the source, and capture the exact error message/code the kernel
# returns.
#
# Why it exists: The `braid replace` optimization plan needs to know: (a) does
# btrfs enforce this size check? (b) what error does it produce? This informs
# whether braid should add an up-front size check for UX, and what the error
# looks like if we don't.
#
# Scenario: 2-disk LUKS+btrfs RAID1 (both 512 MiB). Attempt to replace one
# with a 256 MiB disk. Observe failure. Confirm pool remains intact.

import re

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
luks_format = "--batch-mode --key-file=- --pbkdf pbkdf2 --pbkdf-force-iterations 1000"

# --- Phase 1: Setup — LUKS format + open disk1 and disk2, create RAID1 pool ---

with subtest("Setup: create 2-drive LUKS + btrfs RAID1 pool"):
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

    # Write some data so the pool isn't empty
    machine.succeed("dd if=/dev/urandom of=/mnt/storage/testfile.bin bs=1M count=20")
    machine.succeed("sync")

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print(f"Baseline btrfs fi show:\n{fi_show}")

    # Parse disk2's devid
    disk2_devid = None
    for line in fi_show.splitlines():
        if "/dev/mapper/disk2" in line:
            m = re.search(r"devid\s+(\d+)", line)
            assert m, f"Could not parse devid from line: {line}"
            disk2_devid = m.group(1)
            break
    assert disk2_devid is not None, f"disk2 not found in btrfs fi show:\n{fi_show}"
    print(f"disk2 devid: {disk2_devid}")

# --- Phase 2: LUKS format + open disk3 (the undersized replacement) ---

with subtest("LUKS prep disk3 (256 MiB — smaller than disk2)"):
    dev3 = "/dev/disk/by-id/virtio-disk3"
    machine.succeed(f"echo -n '{passphrase}' | cryptsetup luksFormat {luks_format} {dev3}")
    machine.succeed(f"echo -n '{passphrase}' | cryptsetup luksOpen --key-file=- {dev3} disk3")

# --- Phase 3: Attempt replace — expect failure ---

with subtest("Attempt btrfs replace with smaller target — expect failure"):
    (status, output) = machine.execute(
        f"btrfs replace start -f -B {disk2_devid} /dev/mapper/disk3 /mnt/storage 2>&1"
    )
    print(f"btrfs replace exit code: {status}")
    print(f"btrfs replace output:\n{output}")

    assert status != 0, \
        f"Expected btrfs replace to FAIL with smaller target, but it exited {status}"
    print(f"CONFIRMED: btrfs replace rejected smaller target (exit code {status})")
    print(f"Error message for braid UX reference: {output.strip()}")

# --- Phase 4: Assert pool intact — disk1 and disk2 still present, no corruption ---

with subtest("Pool intact after failed replace"):
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print(f"Pool after failed replace:\n{fi_show}")

    assert "/dev/mapper/disk1" in fi_show, \
        f"disk1 missing from pool after failed replace:\n{fi_show}"
    assert "/dev/mapper/disk2" in fi_show, \
        f"disk2 missing from pool after failed replace:\n{fi_show}"
    assert "/dev/mapper/disk3" not in fi_show, \
        f"disk3 unexpectedly appeared in pool after failed replace:\n{fi_show}"
    assert "missing" not in fi_show.lower(), \
        f"'missing' device in pool after failed replace:\n{fi_show}"
    print("CONFIRMED: pool intact — disk1 and disk2 present, no disk3, no missing")

    # Verify data still readable
    machine.succeed("cat /mnt/storage/testfile.bin > /dev/null")
    print("CONFIRMED: data still readable")

machine.shutdown()
