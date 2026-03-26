# Test: replace --new disk that's already a pool member
#
# Intent:
# - What behavior this test (tries to) verify.
#   - `braid replace --old disk1 --new disk2` fails when disk2 is already
#     a member of the pool. The command exits non-zero, the pool is unchanged,
#     and data is intact.
#
# Why it exists:
# - What risk/regression this protects against.
#   - No explicit braid-level pre-check exists for this case; the failure
#     currently comes from the btrfs layer (device add fails for a device
#     already in the pool). This test documents that behavior and protects
#     against regressions if the error path changes.
#
# Scenario:
# - Real-world situation this models.
#   - Operator makes a typo and specifies an existing pool member as the
#     replacement disk.

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

# --- Phase 1: Attempt replace with --new disk already in pool ---

with subtest("Replace --new already in pool fails"):
    (status, output) = machine.execute(replace_cmd("disk1", "disk2") + " 2>&1")
    print(f"Already-in-pool output (exit {status}):\n{output}")
    assert status != 0, f"Expected failure, got exit 0: {output}"

with subtest("Pool unchanged after failed replace"):
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    for name in ["braid-disk1", "braid-disk2", "braid-disk3"]:
        assert f"/dev/mapper/{name}" in fi_show, f"{name} missing:\n{fi_show}"
    assert "missing" not in fi_show.lower(), f"No missing devices expected:\n{fi_show}"

    devid_count = fi_show.count("devid")
    assert devid_count == 3, f"Expected 3 devices, got {devid_count}:\n{fi_show}"

with subtest("Data intact after failed replace"):
    content = machine.succeed("cat /mnt/storage/precious.txt").strip()
    assert content == "important data", f"Got '{content}'"

with subtest("Disk map unchanged after failed replace"):
    dm = read_disk_map()
    for name in ["disk1", "disk2", "disk3"]:
        assert name in dm["disks"], f"{name} missing from map: {dm}"

machine.shutdown()
