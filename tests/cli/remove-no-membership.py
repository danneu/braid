# Test: braid remove refuses to proceed when pool.json is missing
#
# Intent:
#   `braid remove` must fail hard (exit non-zero) when pool.json does not
#   exist. The btrfs pool must not be touched.
#
# Why it exists:
#   `plan_remove` must not treat MembershipError::NotFound as a warning and
#   continue to btrfs device eviction. That allows the pool to be mutated
#   while pool.json stays absent, creating exactly the state divergence the
#   membership system is meant to prevent.
#
# Scenario:
#   pool.json is accidentally deleted (or was never created during a
#   migration). The operator runs `braid remove` — the command should refuse
#   to proceed without authoritative membership state rather than silently
#   evicting a device.

import shlex

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def add_cmd(name):
    passphrase_q = shlex.quote(passphrase)
    return (
        "printf '%s\\n' " + passphrase_q + " | "
        "braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 " + name + "=/dev/disk/by-id/virtio-" + name + " --passphrase-stdin --yes"
    )


# --- Phase 0: Build 2-drive pool ---

with subtest("Setup: build 2-drive pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    for name in ["braid-disk1", "braid-disk2"]:
        assert "/dev/mapper/" + name in fi_show, name + " missing:\n" + fi_show

    machine.succeed("echo 'important data' > /mnt/storage/precious.txt")
    machine.succeed("sync")

# --- Phase 1: Delete pool.json, then attempt remove ---

with subtest("Remove with missing pool.json fails"):
    machine.succeed("rm /var/lib/braid/pool.json")
    (status, output) = machine.execute("braid remove disk1 --yes 2>&1")
    print("Remove without membership output (exit " + str(status) + "):\n" + output)
    assert status != 0, "Expected failure, got exit 0: " + output

with subtest("Pool unchanged after failed remove"):
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "/dev/mapper/braid-disk1" in fi_show, "disk1 missing:\n" + fi_show
    assert "/dev/mapper/braid-disk2" in fi_show, "disk2 missing:\n" + fi_show
    assert "missing" not in fi_show.lower(), "No missing devices expected:\n" + fi_show

    devid_count = fi_show.count("devid")
    assert devid_count == 2, "Expected 2 devices, got " + str(devid_count) + ":\n" + fi_show

with subtest("Data intact after failed remove"):
    content = machine.succeed("cat /mnt/storage/precious.txt").strip()
    assert content == "important data", "Got '" + content + "'"

machine.shutdown()
