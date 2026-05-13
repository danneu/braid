# Test: scripts/braid-destroy.sh
#
# What: Exercises scripts/braid-destroy.sh end-to-end. Happy path builds a
# 2-disk RAID1 pool, runs the script, and confirms the pool is unmounted,
# mappers closed, LUKS headers wiped, and /var/lib/braid gone. Three
# negative paths each point pool.json at a shape the shell validator must
# reject (old name-keyed shape, missing name, empty by_id, empty .disks,
# missing file), and confirm the script aborts before running braid lock
# or rm -rf.
#
# Why: After commit 74feca5, braid-destroy.sh silently no-op'd the wipe
# loop and left LUKS signatures on every disk while still deleting local
# state. This test pins the fix (pool.json as source of truth, validated
# UUID-keyed schema sniff, by_id/name validation, and reject-before-lock
# ordering) so the regression cannot come back silently.
#
# Scenario: dev blows away a test pool before re-provisioning. Happy path
# must actually destroy; malformed or missing pool.json must abort before
# any env-side change.

import shlex

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def add_cmd(key):
    """Build a `braid add <key> --yes` command with LUKS format args."""
    passphrase_q = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {passphrase_q} | "
        f"braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 {key}=/dev/disk/by-id/virtio-{key} --passphrase-stdin --yes"
    )


def build_pool():
    """Build a 2-disk RAID1 pool; assert live-pool preconditions."""
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed("test -f /var/lib/braid/pool.json")
    machine.succeed("mountpoint -q /mnt/storage")
    machine.succeed("test -e /dev/mapper/braid-disk1")
    machine.succeed("test -e /dev/mapper/braid-disk2")


def write_pool_json(contents):
    """Overwrite /var/lib/braid/pool.json with a literal JSON string."""
    machine.succeed(f"echo {shlex.quote(contents)} > /var/lib/braid/pool.json")


def run_destroy_expect_success():
    """Run the destroy script and require exit 0. Returns combined output."""
    return machine.succeed(
        "echo YES | bash /etc/braid-destroy.sh /etc/braid/config.json 2>&1"
    )


def run_destroy_expect_fail():
    """Run the destroy script and require non-zero exit. Returns combined output."""
    (status, output) = machine.execute(
        "echo YES | bash /etc/braid-destroy.sh /etc/braid/config.json 2>&1"
    )
    assert status != 0, f"expected destroy to fail, got exit 0:\n{output}"
    return output


# --- Scenario 1: happy path ---

with subtest("happy path: destroys live pool"):
    build_pool()
    # Precondition: disks are LUKS-encrypted.
    machine.succeed("cryptsetup isLuks /dev/disk/by-id/virtio-disk1")
    machine.succeed("cryptsetup isLuks /dev/disk/by-id/virtio-disk2")

    run_destroy_expect_success()

    # State directory is gone.
    machine.fail("test -e /var/lib/braid")
    # LUKS headers are wiped from every former pool disk.
    machine.fail("cryptsetup isLuks /dev/disk/by-id/virtio-disk1")
    machine.fail("cryptsetup isLuks /dev/disk/by-id/virtio-disk2")
    # braid lock actually ran: pool unmounted, mappers closed.
    machine.fail("mountpoint -q /mnt/storage")
    machine.fail("test -e /dev/mapper/braid-disk1")
    machine.fail("test -e /dev/mapper/braid-disk2")


# --- Scenario 2: old name-keyed pool.json rejects before braid lock runs ---

with subtest("old name-keyed pool.json rejects before lock"):
    build_pool()
    write_pool_json(
        '{"disks":{"disk1":{"name":"disk1","by_id":"/dev/disk/by-id/virtio-disk1"}}}'
    )

    output = run_destroy_expect_fail()
    assert "is not in UUID-keyed format" in output, (
        f"expected UUID-keyed format error in stderr:\n{output}"
    )

    # Nothing destructive happened.
    machine.succeed("test -e /var/lib/braid")
    machine.succeed("cryptsetup isLuks /dev/disk/by-id/virtio-disk1")
    machine.succeed("cryptsetup isLuks /dev/disk/by-id/virtio-disk2")
    # Load-bearing: mount and mappers untouched proves braid lock never ran.
    machine.succeed("mountpoint -q /mnt/storage")
    machine.succeed("test -e /dev/mapper/braid-disk1")
    machine.succeed("test -e /dev/mapper/braid-disk2")


# --- Scenario 3: missing name rejects before braid lock runs ---

with subtest("missing name rejects before lock"):
    write_pool_json(
        '{"disks":{"11111111-1111-1111-1111-111111111111":{"by_id":"/dev/disk/by-id/virtio-disk1"}}}'
    )

    output = run_destroy_expect_fail()
    assert "no name" in output, f"expected 'no name' in stderr:\n{output}"

    machine.succeed("test -e /var/lib/braid")
    machine.succeed("cryptsetup isLuks /dev/disk/by-id/virtio-disk1")
    machine.succeed("cryptsetup isLuks /dev/disk/by-id/virtio-disk2")
    machine.succeed("mountpoint -q /mnt/storage")
    machine.succeed("test -e /dev/mapper/braid-disk1")
    machine.succeed("test -e /dev/mapper/braid-disk2")


# --- Scenario 4: empty by_id rejects before braid lock runs ---

with subtest("empty by_id rejects before lock"):
    write_pool_json(
        '{"disks":{"11111111-1111-1111-1111-111111111111":{"name":"disk1","by_id":""},"22222222-2222-2222-2222-222222222222":{"name":"disk2","by_id":""}}}'
    )

    output = run_destroy_expect_fail()
    assert "by_id" in output, f"expected 'by_id' in stderr:\n{output}"

    # Nothing destructive happened.
    machine.succeed("test -e /var/lib/braid")
    machine.succeed("cryptsetup isLuks /dev/disk/by-id/virtio-disk1")
    machine.succeed("cryptsetup isLuks /dev/disk/by-id/virtio-disk2")
    # Load-bearing: mount and mappers untouched proves braid lock never ran.
    machine.succeed("mountpoint -q /mnt/storage")
    machine.succeed("test -e /dev/mapper/braid-disk1")
    machine.succeed("test -e /dev/mapper/braid-disk2")


# --- Scenario 5: empty .disks rejects before braid lock runs ---

with subtest("empty .disks rejects before lock"):
    # Reuse the live pool from the earlier negative scenarios.
    write_pool_json('{"disks":{}}')

    output = run_destroy_expect_fail()
    assert "no disks" in output, f"expected 'no disks' in stderr:\n{output}"

    machine.succeed("test -e /var/lib/braid")
    machine.succeed("cryptsetup isLuks /dev/disk/by-id/virtio-disk1")
    machine.succeed("cryptsetup isLuks /dev/disk/by-id/virtio-disk2")
    machine.succeed("mountpoint -q /mnt/storage")
    machine.succeed("test -e /dev/mapper/braid-disk1")
    machine.succeed("test -e /dev/mapper/braid-disk2")


# --- Teardown before Scenario 6: direct primitives, NOT braid lock ---
# A malformed/empty pool.json puts `braid lock` in orphan-mapper territory
# whose behavior is outside this test's contract. Use umount + cryptsetup
# close so a future braid-lock regression does not fail this test for
# unrelated reasons.
machine.succeed("umount /mnt/storage")
machine.succeed("cryptsetup close braid-disk1")
machine.succeed("cryptsetup close braid-disk2")
machine.succeed("rm -rf /var/lib/braid")


# --- Scenario 6: missing pool.json rejects before rm -rf ---

with subtest("missing pool.json rejects before rm -rf"):
    machine.succeed("mkdir -p /var/lib/braid")
    machine.succeed("touch /var/lib/braid/sentinel-no-pool")
    machine.succeed("test ! -f /var/lib/braid/pool.json")

    output = run_destroy_expect_fail()
    assert "no pool to destroy" in output, (
        f"expected 'no pool to destroy' in stderr:\n{output}"
    )
    # Sentinel survives: script did not rm -rf residual state.
    machine.succeed("test -f /var/lib/braid/sentinel-no-pool")
