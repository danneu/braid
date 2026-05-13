# Test: replace --new disk already in pool is caught by braid guard
#
# Intent:
#   `braid replace --old disk1 --new disk2` is rejected with a
#   duplicate-LUKS-UUID membership error when disk2 is already a pool member, AND
#   the rejection leaves no observable side effects:
#     - pool membership and metadata on disk (pool.json) bit-identical
#     - btrfs fi show output unchanged (same devids, no "missing")
#     - user data intact
#     - no stranded pending-op.json (no recovery state left behind)
#
# Why it exists:
#   The live `btrfs replace start` path has no natural duplicate-device
#   guard. Without braid's pre-journal UUID uniqueness guard, the command would
#   reach btrfs and either corrupt the pool or produce a confusing btrfs-level
#   error.
#   Additionally, a preflight failure must not mutate on-disk membership
#   metadata or strand recovery state -- if it did, the user would be
#   forced into `braid recover` for what is conceptually a rejected
#   precondition check.
#
# Scenario:
#   Operator typo -- specifies an existing pool member as --new.

import json
import shlex

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def add_cmd(name):
    passphrase_q = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {passphrase_q} | "
        f"braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 {name}=/dev/disk/by-id/virtio-{name} --passphrase-stdin --yes"
    )


def replace_cmd(old, new):
    passphrase_q = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {passphrase_q} | "
        f"braid replace --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 --old {old} --new {new}=/dev/disk/by-id/virtio-{new} --passphrase-stdin --yes"
    )


def read_pool():
    raw = machine.succeed("cat /var/lib/braid/pool.json")
    return json.loads(raw)


with subtest("Setup: build 2-drive pool with data"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed("echo 'important data' > /mnt/storage/precious.txt")
    machine.succeed("sync")

# Capture baseline state for post-rejection equality checks.
baseline_pool = read_pool()

with subtest("Replace with existing member rejected by braid guard"):
    (status, output) = machine.execute(replace_cmd("disk1", "disk2") + " 2>&1")
    print(f"Guard output (exit {status}):\n{output}")
    assert status != 0, f"Expected non-zero exit, got 0: {output}"
    assert "duplicate LUKS UUID" in output and "already present in membership" in output, (
        f"Expected braid duplicate-UUID membership guard in output, got:\n{output}"
    )

with subtest("Pool unchanged after failed replace"):
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    for name in ["braid-disk1", "braid-disk2"]:
        assert f"/dev/mapper/{name}" in fi_show, f"{name} missing:\n{fi_show}"
    assert "missing" not in fi_show.lower(), (
        f"No missing devices expected:\n{fi_show}"
    )
    devid_count = fi_show.count("devid")
    assert devid_count == 2, f"Expected 2 devices, got {devid_count}:\n{fi_show}"

with subtest("Data intact after failed replace"):
    content = machine.succeed("cat /mnt/storage/precious.txt").strip()
    assert content == "important data", f"Got '{content}'"

with subtest("pool.json bit-identical after failed replace"):
    after_pool = read_pool()
    assert baseline_pool == after_pool, (
        f"pool.json changed after rejected replace.\n"
        f"baseline={baseline_pool}\nafter={after_pool}"
    )

with subtest("No journal stranded after failed replace"):
    machine.fail("test -e /var/lib/braid/pending-op.json")

machine.shutdown()
