# Test: braid remove-missing aborts when the state directory is read-only
#
# Intent:
#   `braid remove-missing` must fail hard (exit non-zero) when the
#   pending-operation journal cannot be written to /var/lib/braid. The
#   btrfs pool must stay intact: the journal write is the first
#   /var/lib/braid write in remove-missing, so a fully read-only state
#   dir aborts the command before pool_remove_device_using runs.
#
# Why it exists:
#   Per ADR-017 (docs/design/decisions/017-runtime-disk-membership.md,
#   "Mutation ordering"), every mutating command writes pending-op.json
#   BEFORE the irreversible btrfs membership change, then writes
#   pool.json AFTER btrfs commits. This test pins the
#   journal-write-fails-first half of that invariant: if
#   journal::write_journal cannot persist pending-op.json (read-only
#   filesystem, ENOSPC, permissions), no btrfs mutation is permitted.
#
#   Scope note: this test does not -- and structurally cannot -- pin
#   the post-mutation pool.json write phase. When the test was added
#   (commit a9b7467), save_membership was the FIRST write in
#   remove-missing and only logged a warning on failure -- the read-only
#   bind mount caught exactly that. Today journal::write_journal
#   precedes the btrfs mutation (`pool_remove_device_using`), and
#   save_membership follows it and propagates errors via `?`. A
#   post-btrfs save_membership failure is a different
#   failure class: btrfs has committed, the journal survives, and
#   `braid recover` is responsible for reconciliation per ADR-017
#   ("Mutation ordering" / recovery model). save_membership's position
#   around btrfs device remove is covered at the unit-test seam by
#   `journal_survives_soft_balance_failure` in
#   cli/src/remove_missing.rs, not here.
#
# Scenario:
#   /var/lib/braid becomes read-only (disk full, permissions issue, or
#   filesystem error) while the operator runs `braid remove-missing`.
#   The journal write fails first, so the command refuses to mutate
#   btrfs.

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


# --- Phase 0: Build 3-drive pool ---

with subtest("Setup: build 3-drive pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed(add_cmd("disk3"))

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    for name in ["braid-disk1", "braid-disk2", "braid-disk3"]:
        assert "/dev/mapper/" + name in fi_show, name + " missing:\n" + fi_show

    machine.succeed("echo 'important data' > /mnt/storage/precious.txt")
    machine.succeed("sync")

# --- Phase 1: Simulate disk3 death and mount degraded ---

with subtest("Simulate disk3 death and mount degraded"):
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup close braid-disk3")
    machine.succeed("mount -o degraded /dev/mapper/braid-disk1 /mnt/storage")

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "missing" in fi_show.lower(), "Expected missing device:\n" + fi_show

# --- Phase 2: Make state directory read-only, then attempt remove-missing ---

with subtest("Make state directory read-only"):
    # atomic_write creates .pending-op.json.tmp in the same directory then
    # renames -- this is the first /var/lib/braid write in remove-missing,
    # so a read-only state dir blocks it before any btrfs mutation.
    # chmod 555 is insufficient -- root bypasses Unix permission bits.
    # A read-only bind mount enforces read-only at the VFS level, blocking
    # even root from creating files in the directory.
    machine.succeed("mount --bind /var/lib/braid /var/lib/braid")
    machine.succeed("mount -o remount,bind,ro /var/lib/braid")

def get_missing_devid():
    """Get the devid of the missing device from braid status --json."""
    import json
    raw = machine.succeed("braid status --json")
    report = json.loads(raw)
    devids = report.get("missing_devids", [])
    assert len(devids) > 0, "No missing devids in braid status:\n" + raw
    return str(devids[0])

missing_devid = get_missing_devid()

with subtest("remove-missing with read-only state directory fails"):
    (status, output) = machine.execute(f"braid remove-missing --missing-id {missing_devid} --yes 2>&1")
    print("remove-missing with readonly dir (exit " + str(status) + "):\n" + output)
    assert status != 0, "Expected failure, got exit 0: " + output

with subtest("Pool still has missing device after failed remove-missing"):
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "missing" in fi_show.lower(), (
        "Missing device should still be present:\n" + fi_show
    )

with subtest("Data intact after failed remove-missing"):
    content = machine.succeed("cat /mnt/storage/precious.txt").strip()
    assert content == "important data", "Got '" + content + "'"

machine.shutdown()
