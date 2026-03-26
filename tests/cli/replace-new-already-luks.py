# Test: replace when new disk is already LUKS-formatted
#
# Intent:
# - What behavior this test (tries to) verify.
#   - When the new disk is already LUKS-formatted (but mapper closed, not in
#     pool), `braid replace` opens the existing LUKS container and proceeds
#     without re-formatting. The replace completes successfully with data
#     intact.
#
# Why it exists:
# - What risk/regression this protects against.
#   - Crash recovery: if a previous `braid replace` crashed after LUKS format
#     but before pool add, retrying should not re-format (destroying the LUKS
#     header). This exercises the `ConfigDiskState::PresentLuks { mapper_open:
#     false }` → `ensure_luks_open` path.
#
# Scenario:
# - Real-world situation this models.
#   - A replace operation is interrupted (power loss, SSH disconnect) after
#     the new disk was LUKS-formatted. Operator retries the replace command.

import json

start_all()
machine.wait_for_unit("multi-user.target")

import shlex

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def read_disk_map():
    raw = machine.succeed("cat /var/lib/braid/disk-map.json")
    return json.loads(raw)


def add_cmd(name):
    passphrase_q = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {passphrase_q} | "
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid add {name}=/dev/disk/by-id/virtio-{name} --passphrase-stdin --yes"
    )


def replace_cmd(old, new, extra=""):
    passphrase_q = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {passphrase_q} | "
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid replace --old {old} --new {new}=/dev/disk/by-id/virtio-{new} --passphrase-stdin --yes {extra}"
    )


# --- Phase 0: Build 3-drive pool ---

with subtest("Setup: build 3-drive pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed(add_cmd("disk3"))

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    for name in ["braid-disk1", "braid-disk2", "braid-disk3"]:
        assert f"/dev/mapper/{name}" in fi_show, f"{name} missing:\n{fi_show}"

    machine.succeed("echo 'important data' > /mnt/storage/precious.txt")
    machine.succeed("sync")

# --- Phase 1: Pre-format disk4 as LUKS, then replace disk2 with it ---

with subtest("Pre-format disk4 as LUKS (simulating crash recovery)"):
    passphrase_q = shlex.quote(passphrase)
    # Use printf '%s' (no newline) to match how braid passes the passphrase
    # to cryptsetup --key-file=- (braid strips the trailing newline).
    machine.succeed(
        f"printf '%s' {passphrase_q} | "
        f"cryptsetup luksFormat --batch-mode --key-file=- {luks_opts} /dev/disk/by-id/virtio-disk4"
    )

    # Verify it's now LUKS
    machine.succeed("cryptsetup isLuks /dev/disk/by-id/virtio-disk4")

    # Capture LUKS UUID before replace — if braid re-formats, this changes
    luks_uuid_before = machine.succeed(
        "cryptsetup luksUUID /dev/disk/by-id/virtio-disk4"
    ).strip()
    print(f"LUKS UUID before replace: {luks_uuid_before}")
    assert luks_uuid_before != "", "Expected non-empty LUKS UUID"

    # Make sure the mapper is NOT open (simulating state after crash)
    machine.fail("test -e /dev/mapper/braid-disk4")

with subtest("Replace disk2 with pre-formatted disk4"):
    result = machine.succeed(replace_cmd("disk2", "disk4"))
    print(f"braid replace output:\n{result}")

with subtest("Pool healthy after replace with pre-formatted disk"):
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print(f"Pool after replace:\n{fi_show}")

    assert "/dev/mapper/braid-disk4" in fi_show, (
        f"disk4 missing from pool:\n{fi_show}"
    )
    assert "braid-disk2" not in fi_show, (
        f"disk2 should be removed:\n{fi_show}"
    )
    assert "missing" not in fi_show.lower(), (
        f"No missing devices expected:\n{fi_show}"
    )

    devid_count = fi_show.count("devid")
    assert devid_count == 3, f"Expected 3 devices, got {devid_count}:\n{fi_show}"

    df_output = machine.succeed("btrfs fi df /mnt/storage")
    assert "RAID1" in df_output, f"Expected RAID1:\n{df_output}"

with subtest("Data intact after replace with pre-formatted disk"):
    content = machine.succeed("cat /mnt/storage/precious.txt").strip()
    assert content == "important data", f"Got '{content}'"

with subtest("LUKS UUID unchanged — disk was NOT re-formatted"):
    luks_uuid_after = machine.succeed(
        "cryptsetup luksUUID /dev/disk/by-id/virtio-disk4"
    ).strip()
    print(f"LUKS UUID after replace: {luks_uuid_after}")
    assert luks_uuid_after == luks_uuid_before, (
        f"LUKS UUID changed — disk was re-formatted! "
        f"before={luks_uuid_before}, after={luks_uuid_after}"
    )

with subtest("Disk map updated"):
    dm = read_disk_map()
    assert "disk2" not in dm["disks"], f"disk2 still in map: {dm}"
    assert "disk4" in dm["disks"], f"disk4 missing from map: {dm}"

machine.shutdown()
