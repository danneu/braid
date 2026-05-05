# Test: braid remove-missing refuses to proceed when membership cannot be saved
#
# Intent:
#   `braid remove-missing` must fail hard (exit non-zero) when pool.json
#   cannot be written. The btrfs pool must not be touched.
#
# Why it exists:
#   remove_missing.rs:158-161 only warns on save_membership failure and
#   proceeds with btrfs device deletion. This allows the missing device to
#   be removed from btrfs while pool.json retains the stale entry, creating
#   exactly the state divergence the membership system is meant to prevent.
#
# Scenario:
#   /var/lib/braid becomes read-only (disk full, permissions issue, or
#   filesystem error) while the operator runs `braid remove-missing`. The
#   command should refuse to mutate btrfs if it cannot persist the
#   membership change first.

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

# --- Phase 2: Make membership dir read-only, then attempt remove-missing ---

with subtest("Make membership dir read-only"):
    # atomic_write creates .pool.json.tmp in the same directory then renames.
    # chmod 555 is insufficient — root bypasses Unix permission bits.
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

with subtest("remove-missing with read-only membership dir fails"):
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
