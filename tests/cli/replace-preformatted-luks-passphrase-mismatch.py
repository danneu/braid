# Test: replace with wrong passphrase on a preformatted, closed LUKS disk
#
# Intent:
# - What behavior this test (tries to) verify.
#   - When the new disk is already LUKS-formatted but closed (mapper not open),
#     `braid replace` must verify the passphrase against the new disk's LUKS
#     header BEFORE writing pending-op.json. A wrong passphrase is a pure
#     reversible preflight failure, so it must not strand the journal or
#     force the user into `braid recover`.
#
# Why it exists:
# - What risk/regression this protects against.
#   - Previously the closed-LUKS path deferred passphrase verification to a
#     post-journal `ensure_luks_open`, so a wrong passphrase left
#     pending-op.json on disk and forced `braid recover` for what was
#     conceptually "command never started." That contradicts decision 019's
#     explicit guidance that a preflight failure aborts cleanly without
#     stranding pending-op.json. See cli/src/replace.rs -- the reversible
#     check near line 214 (`PresentLuks { mapper_open: false }`) pairs with
#     the existing `PresentNotLuks` check one block above.
#   - Complementary to existing coverage:
#       * replace-passphrase-mismatch.py -- wrong passphrase, fresh (non-LUKS)
#         new disk (PresentNotLuks path).
#       * replace-new-already-luks.py -- correct passphrase, preformatted
#         closed-LUKS new disk (success path).
#     This test fills the remaining quadrant: wrong passphrase, preformatted
#     closed-LUKS new disk.
#
# Scenario:
# - Real-world situation this models.
#   - A previous `braid replace` crashed after `cryptsetup luksFormat` but
#     before the pool add. Operator retries, but this time fat-fingers the
#     passphrase. The retry must fail cleanly with no on-disk state change.

import json

start_all()
machine.wait_for_unit("multi-user.target")

import shlex

passphrase = "testpassphrase"
wrong_passphrase = "wrongpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def read_pool():
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

# --- Phase 1: Preformat disk3 as LUKS (crash-recovery starting state) ---

with subtest("Preformat disk3 as LUKS"):
    passphrase_q = shlex.quote(passphrase)
    # printf '%s' (no newline) matches how braid passes the passphrase to
    # cryptsetup --key-file=- (braid strips the trailing newline).
    machine.succeed(
        f"printf '%s' {passphrase_q} | "
        f"cryptsetup luksFormat --batch-mode --key-file=- {luks_opts} /dev/disk/by-id/virtio-disk3"
    )

    machine.succeed("cryptsetup isLuks /dev/disk/by-id/virtio-disk3")

    luks_uuid_before = machine.succeed(
        "cryptsetup luksUUID /dev/disk/by-id/virtio-disk3"
    ).strip()
    assert luks_uuid_before != "", "Expected non-empty LUKS UUID"

    # Mapper must be closed going in (simulates post-crash state).
    machine.fail("test -e /dev/mapper/braid-disk3")

# --- Phase 2: Replace with wrong passphrase ---

with subtest("Replace with wrong passphrase fails"):
    (status, output) = machine.execute(
        replace_cmd_with_passphrase("disk2", "disk3", wrong_passphrase) + " 2>&1"
    )
    print(f"Wrong passphrase output (exit {status}):\n{output}")
    assert status != 0, f"Expected failure, got exit 0: {output}"
    wait_line = "[wait] passphrase: checking against disk1..."
    error_marker = "passphrase does not match existing pool member 'disk1'"
    assert wait_line in output, (
        f"Expected passphrase wait line before retained-member rejection:\n{output}"
    )
    assert output.find(wait_line) < output.find(error_marker), (
        f"Wait line should appear before the retained-member rejection:\n{output}"
    )
    assert "passphrase" in output.lower(), (
        f"Expected passphrase error message:\n{output}"
    )

# --- Phase 3: Bug-fix invariants ---

with subtest("No pending-op.json stranded after failed replace"):
    # The key assertion: the wrong-passphrase preflight failure must NOT
    # leave pending-op.json behind. This is the invariant that regressed
    # before the fix.
    machine.fail("test -e /var/lib/braid/pending-op.json")

with subtest("LUKS UUID unchanged -- disk was not re-formatted"):
    luks_uuid_after = machine.succeed(
        "cryptsetup luksUUID /dev/disk/by-id/virtio-disk3"
    ).strip()
    assert luks_uuid_after == luks_uuid_before, (
        f"LUKS UUID changed -- disk was re-formatted! "
        f"before={luks_uuid_before}, after={luks_uuid_after}"
    )

with subtest("New mapper still closed -- disk was not erroneously opened"):
    machine.fail("test -e /dev/mapper/braid-disk3")

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

with subtest("Pool membership unchanged after failed replace"):
    pm = read_pool()
    assert "disk1" in pm["disks"], f"disk1 missing from pool: {pm}"
    assert "disk2" in pm["disks"], f"disk2 missing from pool: {pm}"
    assert "disk3" not in pm["disks"], f"disk3 should not be in pool: {pm}"

machine.shutdown()
