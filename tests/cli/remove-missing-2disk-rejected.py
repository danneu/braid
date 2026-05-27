# Test: braid remove-missing rejects 2-disk RAID1 + 1 missing at preflight
#
# Intent:
# - What behavior this test verifies.
#   - `braid remove-missing --missing-id <devid>` against a 2-disk
#     RAID1 pool with one disk missing exits non-zero, prints the
#     "2-disk RAID1 pool with one disk missing" reject body naming
#     `braid replace --old <missing-name> --new <new-name>=...`, and leaves no
#     `pending-op.json`
#     behind.
#   - The same reject fires with `--dry-run`, confirming the guard
#     lives in `plan_remove_missing` rather than `execute()`.
#
# Why it exists:
# - What risk/regression this protects against.
#   - The kernel's `btrfs_rm_device` calls
#     `btrfs_check_raid_min_devices(num_devices - 1)` and rejects with
#     `BTRFS_ERROR_DEV_RAID1_MIN_NOT_MET` whenever the remaining count
#     would drop below the RAID1 minimum of 2. Without this preflight,
#     braid strands `pending-op.json` and the sleep inhibitor for a
#     doomed call, then forces the operator into `braid recover` for
#     an operation that was never going to succeed.
#   - The unit test in cli/src/remove_missing.rs pins the wiring; this
#     VM test pins the end-to-end CLI behavior against a real kernel
#     and real btrfs-progs.
#
# Scenario:
# - 2-disk NAS, disk2 dies. Operator reaches for
#   `braid remove-missing` (a reasonable instinct). braid steers them
#   to `braid replace --old <missing-name> --new <new-name>=...` -- the supported
#   repair path documented in docs/design/decisions/012-intent-cli.md.
#
# Missing-disk setup reuses the canonical pattern from
# tests/cli/remove-missing-inhibits-suspend.py: umount -> cryptsetup
# close -> mount -o degraded.

import json
import shlex

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
pq = shlex.quote(passphrase)


def add_cmd(key):
    return (
        f"printf '%s\\n' {pq} | "
        f"braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 "
        f"--luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 "
        f"{key}=/dev/disk/by-id/virtio-{key} --passphrase-stdin --yes"
    )


def get_missing_devid():
    """Get the devid of the missing device from braid status --json."""
    raw = machine.succeed("braid status --json")
    report = json.loads(raw)
    devids = report.get("missing_devids", [])
    assert len(devids) > 0, "No missing devids in braid status:\n" + raw
    return str(devids[0])


# --- Phase 1: Build a 2-disk pool ---

with subtest("Setup: build 2-disk RAID1 pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed("mountpoint -q /mnt/storage")

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    for name in ["braid-disk1", "braid-disk2"]:
        assert "/dev/mapper/" + name in fi_show, name + " missing:\n" + fi_show

# --- Phase 2: Simulate disk2 death and mount degraded ---

with subtest("Simulate disk2 death and mount degraded"):
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup close braid-disk2")
    machine.succeed("mount -o degraded /dev/mapper/braid-disk1 /mnt/storage")

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "missing" in fi_show.lower(), (
        "Expected 'missing' in btrfs filesystem show:\n" + fi_show
    )

# --- Phase 3: Resolve the missing devid (btrfs-authoritative) ---

with subtest("Resolve missing devid"):
    missing_devid = get_missing_devid()
    print(f"missing devid: {missing_devid}")

# --- Phase 4: Real-run reject ---

with subtest("braid remove-missing --yes rejects with the expected body"):
    status, output = machine.execute(
        f"braid remove-missing --missing-id {missing_devid} --yes 2>&1"
    )
    print(f"remove-missing --yes (exit {status}):\n{output}")
    assert status != 0, f"expected non-zero exit, got {status}:\n{output}"
    assert "2-disk RAID1 pool with one disk missing" in output, output
    assert "braid replace" in output, output
    assert (
        "braid replace --old <missing-name> --new <new-name>=/dev/disk/by-id/<...>"
        in output
    ), output
    assert "replace --missing-id" not in output, output

# --- Phase 5: Dry-run reject ---
#
# Ties the dry-run reject to end-to-end coverage as well as the unit
# test. cmd_remove_missing runs plan_remove_missing first and bails on
# Err before reaching `if params.dry_run`, so --dry-run must surface
# the same error -- not a doomed plan preview.

with subtest("braid remove-missing --dry-run rejects with the expected body"):
    status, output = machine.execute(
        f"braid remove-missing --missing-id {missing_devid} --dry-run 2>&1"
    )
    print(f"remove-missing --dry-run (exit {status}):\n{output}")
    assert status != 0, f"expected non-zero exit, got {status}:\n{output}"
    assert "2-disk RAID1 pool with one disk missing" in output, output
    assert "braid replace" in output, output
    assert (
        "braid replace --old <missing-name> --new <new-name>=/dev/disk/by-id/<...>"
        in output
    ), output
    assert "replace --missing-id" not in output, output

# --- Phase 6: No journal stranded ---
#
# The reject must land before journal::write_journal, so
# /var/lib/braid/pending-op.json must not exist after either invocation.

with subtest("No pending-op.json stranded after rejected calls"):
    machine.fail("test -f /var/lib/braid/pending-op.json")

# --- Phase 7: Pool still in its pre-call state ---
#
# Verifies the reject is truly side-effect-free: btrfs still reports
# one missing device, and the surviving disk still mounts.

with subtest("Pool still reports the missing device after reject"):
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "missing" in fi_show.lower(), (
        "missing device should still be present after reject:\n" + fi_show
    )

machine.shutdown()
