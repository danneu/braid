# Test: braid-module-add-locked-pool
#
# Intent: `braid add` for a fresh disk against a locked pool (pool.json
# has members, no mappers open) refuses before any destructive step.
# pool.json is untouched, the new disk has no LUKS header written, and
# the pool stays locked.
#
# Why it exists: regression for the silent-bootstrap bug -- previously,
# `braid add disk3` against a locked 2-disk pool would mkfs.btrfs the
# new disk single-profile, overwrite pool.json, and orphan the existing
# locked members. The user only learned of the data loss when their
# files were missing.
#
# Scenario: 2-disk locked pool (disk1, disk2), plug fresh disk3, operator
# forgets `braid unlock` and runs `braid add disk3=...`. Both --dry-run
# and real run must refuse. The sanity case at the end exercises the
# happy path: unlock, then add disk3 successfully.

import shlex

start_all()
machine.wait_for_unit("multi-user.target", timeout=120)

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def add_cmd(key, *, dry_run=False, capture_stderr=True):
    """Build a `braid add <key>=<by-id>` command shaped like the
    canonical add invocation in tests/cli/braid-add-disk.py: stdin
    passphrase + non-interactive + fast LUKS for VM speed. Without
    --yes --passphrase-stdin, real-run aborts at confirmation before
    reaching LUKS format -- which would let a missing-guard build
    pass these assertions for the wrong reason.

    capture_stderr=True (default) merges stderr into stdout so
    machine.execute's captured output contains the refusal message,
    which braid prints to stderr.
    """
    pq = shlex.quote(passphrase)
    flags = "--passphrase-stdin --yes" + (" --dry-run" if dry_run else "")
    redir = " 2>&1" if capture_stderr else ""
    return (
        f"printf '%s\\n' {pq} | "
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid add {key}=/dev/disk/by-id/virtio-{key} {flags}{redir}"
    )


# Snapshot pool.json before any add attempt so we can prove the file is
# byte-identical after the refusal. Includes the trailing newline (or
# lack thereof) written by systemd.tmpfiles.
pool_json_before = machine.succeed("cat /var/lib/braid/pool.json")

with subtest("disk3 is bare before any add attempt"):
    machine.fail("cryptsetup isLuks /dev/disk/by-id/virtio-disk3")

with subtest("dry-run add against locked pool refuses without rendering plan"):
    rc, out = machine.execute(add_cmd("disk3", dry_run=True))
    assert rc != 0, f"dry-run add must fail; got rc={rc}, out:\n{out}"
    assert "not unlocked" in out, f"expected 'not unlocked' refusal, got:\n{out}"
    assert "disk1" in out, f"refusal must name locked member disk1, got:\n{out}"
    assert "disk2" in out, f"refusal must name locked member disk2, got:\n{out}"
    # Sanity: no plan rendered. Any of these strings would indicate
    # compile_add_steps_multi ran past the new check.
    for forbidden in ["mkfs.btrfs", "luksFormat", "mount /mnt/storage"]:
        assert forbidden not in out, (
            f"refused dry-run must not render any plan steps; "
            f"found {forbidden!r} in:\n{out}"
        )

with subtest("dry-run leaves pool.json and disk3 untouched"):
    pool_json_after_dry = machine.succeed("cat /var/lib/braid/pool.json")
    assert pool_json_after_dry == pool_json_before, (
        f"pool.json must be byte-identical after dry-run refusal\n"
        f"before: {pool_json_before!r}\nafter:  {pool_json_after_dry!r}"
    )
    machine.fail("cryptsetup isLuks /dev/disk/by-id/virtio-disk3")

with subtest("real-run add against locked pool refuses before format/mount"):
    rc, out = machine.execute(add_cmd("disk3"))
    assert rc != 0, f"real-run add must fail; got rc={rc}, out:\n{out}"
    assert "not unlocked" in out, f"expected 'not unlocked' refusal, got:\n{out}"

with subtest("real-run leaves pool.json, disk3, and pool state untouched"):
    pool_json_after_real = machine.succeed("cat /var/lib/braid/pool.json")
    assert pool_json_after_real == pool_json_before, (
        f"pool.json must be byte-identical after real-run refusal\n"
        f"before: {pool_json_before!r}\nafter:  {pool_json_after_real!r}"
    )
    machine.fail("cryptsetup isLuks /dev/disk/by-id/virtio-disk3")
    machine.fail("test -e /dev/mapper/braid-disk3")
    machine.fail("findmnt /mnt/storage")

with subtest("sanity: unlock then add disk3 succeeds"):
    machine.succeed(f"printf '%s\\n' {shlex.quote(passphrase)} | braid unlock --passphrase-stdin")
    machine.succeed("mountpoint /mnt/storage")
    machine.succeed(add_cmd("disk3"))
    fi_show = machine.succeed("btrfs filesystem show /mnt/storage")
    for i in range(1, 4):
        assert f"/dev/mapper/braid-disk{i}" in fi_show, (
            f"disk{i} missing from pool after add:\n{fi_show}"
        )
    pool_json_final = machine.succeed("cat /var/lib/braid/pool.json")
    for name in ["disk1", "disk2", "disk3"]:
        assert f'"{name}"' in pool_json_final, (
            f"pool.json must list {name} after successful add:\n{pool_json_final}"
        )

machine.shutdown()
