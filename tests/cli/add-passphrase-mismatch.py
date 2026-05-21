# Test: add with wrong passphrase is rejected cleanly
#
# Intent:
#   `braid add` with a wrong passphrase fails before any destructive action.
#   The new disk must NOT be LUKS-formatted, the pool must remain unchanged,
#   data must be intact, and pool.json must not contain the new disk.
#
# Why it exists:
#   If save_membership runs before verify_passphrase, a failed add leaves
#   pool.json claiming a disk that was
#   never formatted or added to btrfs. The next `braid unlock` would then
#   target the wrong mapper set, potentially degrading the pool.
#
# Scenario:
#   Operator types the wrong passphrase when adding a third disk to an
#   existing 2-disk pool. The command should fail cleanly with no side
#   effects on disk state or membership.

import json


def member_names(pool):
    return {member["name"] for member in pool["disks"].values()}


def member(pool, name):
    for entry in pool["disks"].values():
        if entry["name"] == name:
            return entry
    raise AssertionError(f"{name} missing from pool.json: {pool}")
import shlex

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
wrong_passphrase = "wrongpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def add_cmd(name):
    passphrase_q = shlex.quote(passphrase)
    return (
        "printf '%s\\n' " + passphrase_q + " | "
        "braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 " + name + "=/dev/disk/by-id/virtio-" + name + " --passphrase-stdin --yes"
    )


def add_cmd_wrong_passphrase(name):
    passphrase_q = shlex.quote(wrong_passphrase)
    return (
        "printf '%s\\n' " + passphrase_q + " | "
        "braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 " + name + "=/dev/disk/by-id/virtio-" + name + " --passphrase-stdin --yes"
    )


def read_membership():
    raw = machine.succeed("cat /var/lib/braid/pool.json")
    return json.loads(raw)


# --- Phase 0: Build 2-drive pool ---

with subtest("Setup: build 2-drive pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    for name in ["braid-disk1", "braid-disk2"]:
        assert "/dev/mapper/" + name in fi_show, name + " missing:\n" + fi_show

    machine.succeed("echo 'important data' > /mnt/storage/precious.txt")
    machine.succeed("sync")

# --- Phase 1: Attempt add with wrong passphrase ---

with subtest("Add with wrong passphrase fails"):
    (status, output) = machine.execute(
        add_cmd_wrong_passphrase("disk3") + " 2>&1"
    )
    print("Wrong passphrase output (exit " + str(status) + "):\n" + output)
    assert status != 0, "Expected failure, got exit 0: " + output
    wait_line = "[wait] passphrase: checking against disk1..."
    error_marker = "passphrase does not match existing pool member"
    assert wait_line in output, (
        "Expected passphrase wait line before mismatch error:\n" + output
    )
    assert output.find(wait_line) < output.find(error_marker), (
        "Wait line should appear before the mismatch error:\n" + output
    )
    assert "passphrase" in output.lower(), (
        "Expected passphrase error message:\n" + output
    )

with subtest("New disk is NOT LUKS-formatted after failed add"):
    (status, _) = machine.execute("cryptsetup isLuks /dev/disk/by-id/virtio-disk3")
    assert status != 0, "disk3 should NOT be LUKS-formatted after passphrase mismatch"

with subtest("Pool unchanged after failed add"):
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "/dev/mapper/braid-disk1" in fi_show, "disk1 missing:\n" + fi_show
    assert "/dev/mapper/braid-disk2" in fi_show, "disk2 missing:\n" + fi_show
    assert "braid-disk3" not in fi_show, "disk3 should not be in pool:\n" + fi_show
    assert "missing" not in fi_show.lower(), "No missing devices expected:\n" + fi_show

    devid_count = fi_show.count("devid")
    assert devid_count == 2, "Expected 2 devices, got " + str(devid_count) + ":\n" + fi_show

with subtest("Data intact after failed add"):
    content = machine.succeed("cat /mnt/storage/precious.txt").strip()
    assert content == "important data", "Got '" + content + "'"

with subtest("Membership unchanged after failed add"):
    m = read_membership()
    assert "disk1" in member_names(m), "disk1 missing from membership: " + str(m)
    assert "disk2" in member_names(m), "disk2 missing from membership: " + str(m)
    assert "disk3" not in member_names(m), "disk3 should not be in membership: " + str(m)

machine.shutdown()
