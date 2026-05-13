# Test: bootstrap crash recovery (escape instructions)
#
# Intent: Verify `braid recover` detects a bootstrap crash and prints
#   actionable escape instructions instead of a cryptic mount error.
#
# Why it exists: recover.rs:62-110 has special handling for bootstrap crashes
#   (empty pre_membership + MountFailed + NoBtrfs probe on target disks).
#   This path is covered by 4 Rust unit tests with mocked runners, but no VM
#   test exercises it end-to-end against real LUKS + btrfs filesystem show.
#   A regression here would leave first-time users stranded with an opaque
#   mount error and no documented way out.
#
# Scenario: A single-disk bootstrap add was interrupted after LUKS format
#   but before mkfs.btrfs. The disk has a valid LUKS header but no btrfs
#   filesystem inside. A pending-op.json with empty pre_membership is
#   injected. `braid recover` opens LUKS, fails to mount (no superblock),
#   probes the mapper with `btrfs filesystem show`, confirms NoBtrfs, and
#   prints escape instructions naming the pending-op.json path, the disk's
#   by_id path, and wipefs.

import json
import shlex

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
pq = shlex.quote(passphrase)
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"
disk1_uuid = "11111111-1111-1111-1111-111111111111"

# --- Phase 1: Simulate interrupted bootstrap (LUKS format, no mkfs) ---

with subtest("LUKS format disk1 without mkfs"):
    machine.succeed(
        f"printf '%s' {pq} | cryptsetup luksFormat {luks_opts} "
        f"--uuid {disk1_uuid} --label braid-disk1 "
        f"/dev/disk/by-id/virtio-disk1 -"
    )
    # Verify LUKS header exists, then leave it closed
    machine.succeed("cryptsetup isLuks /dev/disk/by-id/virtio-disk1")

# --- Phase 2: Inject pending-op.json ---

with subtest("Inject bootstrap journal"):
    machine.succeed("mkdir -p /var/lib/braid")

    journal = {
        "started_at": "2026-01-01T00:00:00Z",
        "op": {
            "op": "Add",
            "phase": "PoolMutation",
            "targets": {
                disk1_uuid: {
                    "name": "disk1",
                    "by_id": "/dev/disk/by-id/virtio-disk1",
                    "mode": {
                        "FreshLuks": {
                            "extra_opts": [],
                            "enroll_key_file": None,
                        }
                    },
                }
            },
        },
        "pre_membership": {"disks": {}},
        "target_membership": {
            "disks": {
                disk1_uuid: {
                    "name": "disk1",
                    "by_id": "/dev/disk/by-id/virtio-disk1",
                },
            }
        },
    }
    journal_json = json.dumps(journal)
    machine.succeed(
        f"cat > /var/lib/braid/pending-op.json << 'JOURNAL_EOF'\n"
        f"{journal_json}\n"
        f"JOURNAL_EOF"
    )
    machine.succeed("test -f /var/lib/braid/pending-op.json")

    # Ensure no pool.json exists (fresh system)
    machine.succeed("test ! -f /var/lib/braid/pool.json")

# --- Phase 3: braid recover must fail with escape instructions ---

with subtest("braid recover prints bootstrap escape instructions"):
    exit_code, output = machine.execute(
        f"printf '%s\\n' {pq} | braid recover --passphrase-stdin 2>&1"
    )
    assert exit_code != 0, (
        f"braid recover should fail for bootstrap crash, but got exit {exit_code}"
    )
    assert "bootstrap add was interrupted" in output, (
        f"Expected 'bootstrap add was interrupted' in output, got: {output}"
    )
    assert "pending-op.json" in output, (
        f"Expected 'pending-op.json' in output, got: {output}"
    )
    assert "wipefs" in output, (
        f"Expected 'wipefs' in output, got: {output}"
    )
    assert "virtio-disk1" in output, (
        f"Expected disk1 by_id path in output, got: {output}"
    )

# --- Phase 4: Verify state ---

with subtest("LUKS mapper was closed after failed recover"):
    machine.fail("test -e /dev/mapper/braid-disk1")

with subtest("No state mutation after bootstrap crash"):
    # Journal preserved — user needs it to follow escape instructions
    machine.succeed("test -f /var/lib/braid/pending-op.json")

    # pool.json must NOT exist — no pool was ever created
    machine.fail("test -f /var/lib/braid/pool.json")

    # Pool must NOT be mounted
    machine.fail("mountpoint -q /mnt/storage")

machine.shutdown()
