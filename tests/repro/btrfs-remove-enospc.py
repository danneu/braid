# Repro: btrfs device remove missing — instant ENOSPC
#
# Intent: Reproduce and document btrfs failure mode 1. When a pool is
# completely full and a device goes missing, `btrfs device remove missing`
# fails immediately with "No space left on device". The filesystem
# survives — no crash, no read-only.
#
# Why it exists: Documents the "safe" failure mode for comparison with
# the catastrophic crash in btrfs-remove-enospc-crash. Both must be
# prevented by braid's pre-flight space check.
#
# Scenario: 3×512MiB RAID1 pool, filled to 100%, one drive dies.
# Surviving drives have zero unallocated space. btrfs can't even begin
# block group relocation — fails instantly.

import shlex

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def add_cmd(key):
    passphrase_q = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {passphrase_q} | "
        f"braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 {key}=/dev/disk/by-id/virtio-{key} --passphrase-stdin --yes"
    )


# --- Phase 1: Build 3-drive RAID1 pool ---

with subtest("Setup: build 3-drive pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed(add_cmd("disk3"))

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    for name in ["braid-disk1", "braid-disk2", "braid-disk3"]:
        assert f"/dev/mapper/{name}" in fi_show, f"{name} missing:\n{fi_show}"

# --- Phase 2: Fill pool to 100% ---

with subtest("Fill pool completely"):
    machine.succeed("dd if=/dev/zero of=/mnt/storage/fill1 bs=1M count=200 status=progress")
    machine.succeed("sync")
    machine.succeed("dd if=/dev/zero of=/mnt/storage/fill2 bs=1M count=200 status=progress")
    machine.succeed("sync")
    machine.execute("dd if=/dev/zero of=/mnt/storage/fill3 bs=1M count=200 status=progress")
    machine.succeed("sync")

    dev_usage = machine.succeed("btrfs device usage --raw /mnt/storage")
    print(f"Device usage after fill:\n{dev_usage}")

# --- Phase 3: Simulate disk death, mount degraded ---

with subtest("Simulate disk3 death and mount degraded"):
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup close braid-disk3")
    machine.succeed("mount -o degraded /dev/mapper/braid-disk1 /mnt/storage")

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print(f"Pool after death:\n{fi_show}")
    assert "missing" in fi_show.lower()

# --- Phase 4: btrfs device remove fails instantly with ENOSPC ---

with subtest("btrfs device remove missing fails with ENOSPC"):
    (status, output) = machine.execute(
        "btrfs device remove missing /mnt/storage 2>&1"
    )
    print(f"btrfs device remove output (exit {status}):\n{output}")
    assert status != 0, f"Expected failure, got exit 0: {output}"
    assert "no space left" in output.lower(), \
        f"Expected ENOSPC error:\n{output}"

# --- Phase 5: Filesystem survived — still writable ---

with subtest("Filesystem still writable after instant ENOSPC"):
    machine.succeed("touch /mnt/storage/test-write")
    machine.succeed("rm /mnt/storage/test-write")

machine.shutdown()
