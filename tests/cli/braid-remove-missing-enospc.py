# Test: braid remove-missing — ENOSPC pre-flight rejection
#
# Intent: Verify that `braid remove-missing --yes` rejects the operation
# when surviving devices lack free space to absorb the missing device's
# allocations.
#
# Why it exists: btrfs device remove with insufficient space either fails
# instantly (ENOSPC) or — catastrophically — starts relocating, hits ENOSPC
# mid-transaction, and forces the filesystem read-only. The pre-flight
# check prevents both by comparing allocations vs free space before
# invoking btrfs.
#
# Scenario: 3×512MiB RAID1 pool filled to ~100%, one drive dies, then
# `braid remove-missing --yes` is called. The command must fail with a
# clear error about insufficient space, leave the pool unchanged (still
# has missing device), and keep the filesystem writable.

import json
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

# --- Phase 3: Simulate disk death, mount degraded ---

with subtest("Simulate disk3 death and mount degraded"):
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup close braid-disk3")
    machine.succeed("mount -o degraded /dev/mapper/braid-disk1 /mnt/storage")

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print(f"Pool after death:\n{fi_show}")
    assert "missing" in fi_show.lower()

def get_missing_devid():
    """Get the devid of the missing device from braid status --json."""
    raw = machine.succeed("braid status --json")
    report = json.loads(raw)
    devids = report.get("missing_devids", [])
    assert len(devids) > 0, "No missing devids in braid status:\n" + raw
    return str(devids[0])

# --- Phase 4: braid remove-missing rejects with space error ---

missing_devid = get_missing_devid()

with subtest("braid remove-missing rejects due to insufficient space"):
    (status, output) = machine.execute(
        f"braid remove-missing --missing-id {missing_devid} --yes 2>&1"
    )
    print(f"braid remove-missing output (exit {status}):\n{output}")
    assert status != 0, f"Expected failure, got exit 0: {output}"

    output_lower = output.lower()
    assert "not enough space" in output_lower, \
        f"Expected 'not enough space' in error:\n{output}"

# --- Phase 5: Pool unchanged — still has missing device ---

with subtest("Pool still has missing device (unchanged)"):
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print(f"Pool after rejection:\n{fi_show}")
    assert "missing" in fi_show.lower(), \
        f"Expected pool to still show missing device:\n{fi_show}"

# --- Phase 6: Filesystem still writable ---

with subtest("Filesystem still writable after rejection"):
    machine.succeed("touch /mnt/storage/test-write")
    machine.succeed("rm /mnt/storage/test-write")

machine.shutdown()
