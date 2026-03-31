# Test: braid remove — ENOSPC pre-flight rejection (live disk)
#
# Intent: Verify that `braid remove disk3 --yes` rejects when remaining
# devices lack free space to absorb the target device's data.
#
# Why it exists: `braid remove` calls `btrfs device remove` which
# relocates data off the target device. Same ENOSPC risk as remove-missing:
# either instant failure or filesystem crash to read-only mid-relocation.
# The pre-flight check prevents both.
#
# Scenario: 3×512MiB RAID1 pool filled to ~100%, all disks alive.
# `braid remove disk3 --yes` must fail with a clear error about
# insufficient space, leave the pool unchanged, and keep the filesystem
# writable.

import shlex

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def add_cmd(key):
    passphrase_q = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {passphrase_q} | "
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid add {key}=/dev/disk/by-id/virtio-{key} --passphrase-stdin --yes"
    )


# --- Phase 1: Build 3-drive RAID1 pool ---

with subtest("Setup: build 3-drive pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed(add_cmd("disk3"))

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    for name in ["braid-disk1", "braid-disk2", "braid-disk3"]:
        assert f"/dev/mapper/{name}" in fi_show, f"{name} missing:\n{fi_show}"

# --- Phase 2: Fill pool to ~100% ---

with subtest("Fill pool completely"):
    machine.succeed("dd if=/dev/zero of=/mnt/storage/fill1 bs=1M count=200 status=progress")
    machine.succeed("sync")
    machine.succeed("dd if=/dev/zero of=/mnt/storage/fill2 bs=1M count=200 status=progress")
    machine.succeed("sync")
    machine.execute("dd if=/dev/zero of=/mnt/storage/fill3 bs=1M count=200 status=progress")
    machine.succeed("sync")

    dev_usage = machine.succeed("btrfs device usage --raw /mnt/storage")
    print(f"Device usage after fill:\n{dev_usage}")

# --- Phase 3: braid remove rejects with space error ---

with subtest("braid remove rejects due to insufficient space"):
    (status, output) = machine.execute(
        "braid remove disk3 --yes 2>&1"
    )
    print(f"braid remove output (exit {status}):\n{output}")
    assert status != 0, f"Expected failure, got exit 0: {output}"

    output_lower = output.lower()
    assert "not enough space" in output_lower, \
        f"Expected 'not enough space' in error:\n{output}"

# --- Phase 4: Pool unchanged — still has all 3 devices ---

with subtest("Pool still has all 3 devices (unchanged)"):
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print(f"Pool after rejection:\n{fi_show}")
    for name in ["braid-disk1", "braid-disk2", "braid-disk3"]:
        assert f"/dev/mapper/{name}" in fi_show, \
            f"{name} missing after rejection:\n{fi_show}"

# --- Phase 5: Filesystem still writable ---

with subtest("Filesystem still writable after rejection"):
    machine.succeed("touch /mnt/storage/test-write")
    machine.succeed("rm /mnt/storage/test-write")

machine.shutdown()
