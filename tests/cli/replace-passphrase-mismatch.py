# Test: replace with wrong passphrase is rejected
#
# Intent:
# - What behavior this test (tries to) verify.
#   - `braid replace` with a wrong passphrase fails before any destructive
#     action. The new disk must NOT be LUKS-formatted, the pool must remain
#     unchanged, and data must be intact.
#
# Why it exists:
# - What risk/regression this protects against.
#   - If passphrase verification happened after LUKS format, the new disk
#     would be encrypted with a mismatched passphrase, creating an
#     inaccessible disk. The `verify_passphrase` check at replace.rs:176
#     must fire before `luks_format`.
#
# Scenario:
# - Real-world situation this models.
#   - Operator types the wrong passphrase when replacing a disk. The command
#     should fail cleanly with no side effects.

import json

start_all()
machine.wait_for_unit("multi-user.target")

import shlex

passphrase = "testpassphrase"
wrong_passphrase = "wrongpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def read_disk_map():
    raw = machine.succeed("cat /var/lib/braid/disk-map.json")
    return json.loads(raw)


def read_membership():
    raw = machine.succeed("cat /var/lib/braid/pool.json")
    return json.loads(raw)


def add_cmd(name):
    passphrase_q = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {passphrase_q} | "
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid add {name}=/dev/disk/by-id/virtio-{name} --passphrase-stdin --yes"
    )


def replace_cmd_with_passphrase(old, new, pp):
    passphrase_q = shlex.quote(pp)
    return (
        f"printf '%s\\n' {passphrase_q} | "
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid replace --old {old} --new {new}=/dev/disk/by-id/virtio-{new} --passphrase-stdin --yes"
    )


# --- Phase 0: Build 2-drive pool ---

with subtest("Setup: build 2-drive pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    for name in ["braid-disk1", "braid-disk2"]:
        assert f"/dev/mapper/{name}" in fi_show, f"{name} missing:\n{fi_show}"

    machine.succeed("echo 'important data' > /mnt/storage/precious.txt")
    machine.succeed("sync")

# --- Phase 1: Attempt replace with wrong passphrase ---

with subtest("Replace with wrong passphrase fails"):
    (status, output) = machine.execute(
        replace_cmd_with_passphrase("disk1", "disk3", wrong_passphrase) + " 2>&1"
    )
    print(f"Wrong passphrase output (exit {status}):\n{output}")
    assert status != 0, f"Expected failure, got exit 0: {output}"
    assert "passphrase" in output.lower(), (
        f"Expected passphrase error message:\n{output}"
    )

with subtest("New disk is NOT LUKS-formatted after failed replace"):
    (status, _) = machine.execute("cryptsetup isLuks /dev/disk/by-id/virtio-disk3")
    assert status != 0, "disk3 should NOT be LUKS-formatted after passphrase mismatch"

with subtest("Pool unchanged after failed replace"):
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "/dev/mapper/braid-disk1" in fi_show, f"disk1 missing:\n{fi_show}"
    assert "/dev/mapper/braid-disk2" in fi_show, f"disk2 missing:\n{fi_show}"
    assert "braid-disk3" not in fi_show, f"disk3 should not be in pool:\n{fi_show}"
    assert "missing" not in fi_show.lower(), f"No missing devices expected:\n{fi_show}"

    devid_count = fi_show.count("devid")
    assert devid_count == 2, f"Expected 2 devices, got {devid_count}:\n{fi_show}"

with subtest("Data intact after failed replace"):
    content = machine.succeed("cat /mnt/storage/precious.txt").strip()
    assert content == "important data", f"Got '{content}'"

with subtest("Disk map unchanged after failed replace"):
    dm = read_disk_map()
    assert "disk1" in dm["disks"], f"disk1 missing from map: {dm}"
    assert "disk2" in dm["disks"], f"disk2 missing from map: {dm}"
    assert "disk3" not in dm["disks"], f"disk3 should not be in map: {dm}"

with subtest("Membership unchanged after failed replace"):
    m = read_membership()
    assert "disk1" in m["disks"], (
        "disk1 missing from membership: " + str(m)
    )
    assert "disk2" in m["disks"], (
        "disk2 missing from membership: " + str(m)
    )
    assert "disk3" not in m["disks"], (
        "disk3 should not be in membership: " + str(m)
    )

machine.shutdown()
