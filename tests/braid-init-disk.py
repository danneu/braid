import json

# Test: braid init-disk
#
# Phase 1: Command dispatch — init-disk exists, parses args, shows help.
# Phase 2 (section 11.1): Full safety contract tests.

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def write_config(disk_list, mount="/mnt/storage"):
    config = json.dumps({"disks": disk_list, "mountPoint": mount})
    escaped = config.replace("'", "'\\''")
    return f"echo '{escaped}' > /tmp/braid-config.json"


def init_disk(by_id, extra="", confirm=""):
    env = f"BRAID_PASSPHRASE='{passphrase}' BRAID_LUKS_OPTS='{luks_opts}'"
    if confirm:
        env += f" BRAID_CONFIRM='{confirm}'"
    return f"{env} braid init-disk --config /tmp/braid-config.json {by_id} {extra}"


# ============================================================================
# Phase 1: Command dispatch
# ============================================================================

with subtest("init-disk shows help"):
    output = machine.succeed("braid init-disk --help")
    assert "Usage:" in output, f"Missing usage in help:\n{output}"
    assert "init-disk" in output, f"Missing init-disk in help:\n{output}"
    assert "--force" in output, f"Missing --force in help:\n{output}"

with subtest("init-disk requires by-id argument"):
    machine.succeed(write_config(["/dev/disk/by-id/virtio-disk1"]))
    machine.fail("braid init-disk --config /tmp/braid-config.json")

with subtest("init-disk rejects unknown options"):
    machine.fail("braid init-disk --config /tmp/braid-config.json --bogus /dev/disk/by-id/virtio-disk1")

with subtest("braid --help lists init-disk"):
    output = machine.succeed("braid --help 2>&1 || true")
    assert "init-disk" in output, f"init-disk not in help:\n{output}"

# ============================================================================
# Phase 2: Section 11.1 — init-disk safety contract
# ============================================================================

# --- 11.1.1: init-disk formats declared non-LUKS disk ---

with subtest("init-disk formats declared non-LUKS disk"):
    machine.succeed(write_config(["/dev/disk/by-id/virtio-disk1"]))
    machine.succeed(init_disk("/dev/disk/by-id/virtio-disk1"))

    # Verify disk now has LUKS header
    machine.succeed("cryptsetup isLuks /dev/disk/by-id/virtio-disk1")

# --- 11.1.2: init-disk refuses undeclared disk ---

with subtest("init-disk refuses undeclared disk"):
    # disk2 is NOT in config
    machine.succeed(write_config(["/dev/disk/by-id/virtio-disk1"]))
    machine.fail(init_disk("/dev/disk/by-id/virtio-disk2"))

# --- 11.1.3: init-disk refuses already-LUKS disk without --force ---

with subtest("init-disk refuses already-LUKS disk without --force"):
    # disk1 was formatted above — it has a LUKS header now
    machine.succeed(write_config(["/dev/disk/by-id/virtio-disk1"]))
    machine.fail(init_disk("/dev/disk/by-id/virtio-disk1"))

# --- 11.1.4: init-disk --force requires confirmation phrase ---

with subtest("init-disk --force without confirmation fails"):
    machine.succeed(write_config(["/dev/disk/by-id/virtio-disk1"]))
    machine.fail(init_disk("/dev/disk/by-id/virtio-disk1", "--force"))

with subtest("init-disk --force with wrong confirmation fails"):
    machine.fail(init_disk("/dev/disk/by-id/virtio-disk1", "--force", confirm="wrong phrase"))

with subtest("init-disk --force with correct confirmation succeeds"):
    machine.succeed(init_disk("/dev/disk/by-id/virtio-disk1", "--force", confirm="reformat this disk"))
    machine.succeed("cryptsetup isLuks /dev/disk/by-id/virtio-disk1")

# --- 11.1.5: init-disk refuses formatting disk currently in pool ---

with subtest("init-disk refuses disk currently in pool"):
    # Set up a pool with disk1
    machine.succeed(write_config([
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk2",
    ]))
    machine.succeed(
        f"echo -n '{passphrase}' | cryptsetup luksOpen --key-file=- "
        "/dev/disk/by-id/virtio-disk1 virtio-disk1"
    )
    machine.succeed("mkfs.btrfs -f /dev/mapper/virtio-disk1")
    machine.succeed("mkdir -p /mnt/storage && mount /dev/mapper/virtio-disk1 /mnt/storage")

    # Now try to init-disk disk1 while it's in the pool — should refuse
    machine.fail(init_disk("/dev/disk/by-id/virtio-disk1", "--force", confirm="reformat this disk"))

# --- 11.1.6: init-disk enforces single-passphrase check ---

with subtest("init-disk enforces single-passphrase check against existing pool member"):
    # disk1 is formatted with passphrase "testpassphrase" and mounted in pool
    # Try to init disk2 with a WRONG passphrase — should fail
    wrong_pass_cmd = (
        f"BRAID_PASSPHRASE='wrongpassphrase' "
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid init-disk --config /tmp/braid-config.json /dev/disk/by-id/virtio-disk2"
    )
    machine.fail(wrong_pass_cmd)

    # With correct passphrase — should succeed
    machine.succeed(init_disk("/dev/disk/by-id/virtio-disk2"))
    machine.succeed("cryptsetup isLuks /dev/disk/by-id/virtio-disk2")


# Clean up
machine.succeed("umount /mnt/storage || true")
machine.succeed("cryptsetup close virtio-disk1 || true")

machine.shutdown()
