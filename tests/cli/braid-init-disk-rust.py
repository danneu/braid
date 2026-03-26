import json

# Test: braid init-disk (Rust implementation)
#
# Mirrors the bash init-disk test (14-braid-init-disk) using braid.
# Phase 1: Safety contract — all gates must hold.

start_all()
machine.wait_for_unit("multi-user.target")

import shlex

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def write_config(mount="/mnt/storage"):
    config = json.dumps({"mount_point": mount})
    escaped = config.replace("'", "'\\''")
    return f"echo '{escaped}' > /tmp/braid-config.json"


def init_disk(by_id, extra="", confirm=""):
    passphrase_q = shlex.quote(passphrase)
    env = f"BRAID_LUKS_OPTS='{luks_opts}'"
    if confirm:
        env += f" BRAID_CONFIRM='{confirm}'"
    return f"printf '%s\\n' {passphrase_q} | {env} braid init-disk --config /tmp/braid-config.json --passphrase-stdin {extra} {by_id}"


# ============================================================================
# Phase 1: Safety contract
# ============================================================================

# --- Formats declared non-LUKS disk ---

with subtest("init-disk formats declared non-LUKS disk"):
    machine.succeed(write_config())
    machine.succeed(init_disk("/dev/disk/by-id/virtio-disk1"))

    # Verify disk now has LUKS header
    machine.succeed("cryptsetup isLuks /dev/disk/by-id/virtio-disk1")

# --- Refuses already-LUKS without --force ---

with subtest("init-disk refuses already-LUKS disk without --force"):
    # disk1 was formatted above
    machine.succeed(write_config())
    machine.fail(init_disk("/dev/disk/by-id/virtio-disk1"))

# --- --force without BRAID_CONFIRM fails ---

with subtest("init-disk --force without confirmation fails"):
    machine.succeed(write_config())
    machine.fail(init_disk("/dev/disk/by-id/virtio-disk1", "--force"))

# --- --force with wrong BRAID_CONFIRM fails ---

with subtest("init-disk --force with wrong confirmation fails"):
    machine.fail(init_disk("/dev/disk/by-id/virtio-disk1", "--force", confirm="wrong phrase"))

# --- --force with correct BRAID_CONFIRM succeeds ---

with subtest("init-disk --force with correct confirmation succeeds"):
    machine.succeed(init_disk("/dev/disk/by-id/virtio-disk1", "--force", confirm="reformat this disk"))
    machine.succeed("cryptsetup isLuks /dev/disk/by-id/virtio-disk1")

# --- Refuses disk currently in pool ---

with subtest("init-disk refuses disk currently in pool"):
    # Set up a pool with disk1
    machine.succeed(write_config())
    machine.succeed(
        f"echo -n '{passphrase}' | cryptsetup luksOpen --key-file=- "
        "/dev/disk/by-id/virtio-disk1 virtio-disk1"
    )
    machine.succeed("mkfs.btrfs -f -d single -m dup /dev/mapper/virtio-disk1")
    machine.succeed("mkdir -p /mnt/storage && mount /dev/mapper/virtio-disk1 /mnt/storage")

    # Now try to init-disk disk1 while it's in the pool — should refuse
    machine.fail(init_disk("/dev/disk/by-id/virtio-disk1", "--force", confirm="reformat this disk"))

# --- Wrong passphrase against existing member fails ---

with subtest("wrong passphrase against existing member fails"):
    # disk1 is formatted with passphrase "testpassphrase" and mounted in pool
    wrong_pass_cmd = (
        f"printf '%s\\n' wrongpassphrase | "
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid init-disk --config /tmp/braid-config.json --passphrase-stdin /dev/disk/by-id/virtio-disk2"
    )
    machine.fail(wrong_pass_cmd)

# --- Correct passphrase succeeds ---

with subtest("correct passphrase succeeds"):
    machine.succeed(init_disk("/dev/disk/by-id/virtio-disk2"))
    machine.succeed("cryptsetup isLuks /dev/disk/by-id/virtio-disk2")


# Clean up
machine.succeed("umount /mnt/storage || true")
machine.succeed("cryptsetup close virtio-disk1 || true")

machine.shutdown()
