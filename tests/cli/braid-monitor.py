# Test: braid monitor + ack lifecycle
#
# Intent: Verify the full alert lifecycle for btrfs-detected issues:
#   detection → status banner → ack → cleared.
#
# Why it exists: Without this test, we have no integration proof that
#   `braid monitor` exit codes, `braid status` banners, and `braid ack`
#   all agree on the alert state.
#
# Scenario: 3-disk RAID1 pool. One LUKS mapper is closed to simulate a
#   failed drive. monitor detects the degraded state, status shows the
#   banner, ack clears it, and monitor returns clean.

import json

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"

# --- Setup: create 3-disk RAID1 pool ---
with subtest("Create 3-disk RAID1 pool"):
    for d in ["disk1", "disk2", "disk3"]:
        machine.succeed(
            f"echo -n '{passphrase}' | cryptsetup luksFormat --batch-mode --key-file=- --pbkdf pbkdf2 --pbkdf-force-iterations 1000 /dev/disk/by-id/virtio-{d}"
        )
        machine.succeed(
            f"echo -n '{passphrase}' | cryptsetup open --type luks --key-file=- /dev/disk/by-id/virtio-{d} braid-{d}"
        )

    machine.succeed(
        "mkfs.btrfs -f -d raid1 -m raid1 /dev/mapper/braid-disk1 /dev/mapper/braid-disk2 /dev/mapper/braid-disk3"
    )
    machine.succeed("mkdir -p /mnt/storage")
    machine.succeed("mount /dev/mapper/braid-disk1 /mnt/storage")
    machine.succeed("mkdir -p /var/lib/braid")

with subtest("Healthy pool: monitor exits 0"):
    machine.succeed("braid monitor")

with subtest("Healthy pool: status has no ALERT"):
    output = machine.succeed("braid status")
    assert "ALERT" not in output, f"Expected no ALERT in healthy status, got: {output}"

# --- Simulate disk failure: close one LUKS mapper ---
with subtest("Simulate disk failure"):
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup close braid-disk2")
    # Remount degraded (only 2 of 3 devices)
    machine.succeed("mount -o degraded /dev/mapper/braid-disk1 /mnt/storage")

with subtest("Degraded pool: monitor exits 1"):
    machine.fail("braid monitor")

with subtest("Degraded pool: status shows ALERT banner"):
    output = machine.succeed("braid status")
    assert "ALERT" in output, f"Expected ALERT in degraded status, got: {output}"
    assert "braid ack" in output, f"Expected 'braid ack' hint in status, got: {output}"
    assert "missing device" in output, f"Expected 'missing device' cause in status, got: {output}"

with subtest("Degraded pool: status --json shows alert"):
    json_output = machine.succeed("braid status --json")
    report = json.loads(json_output)
    assert report["alert_active"] == True, f"Expected alert_active=true, got: {report}"
    cause_types = [c["type"] for c in report["alert_causes"]]
    assert "missing_device" in cause_types, f"Expected missing_device cause, got: {cause_types}"

with subtest("Ack clears alert"):
    machine.succeed("braid ack")
    # Verify acked state file was written
    machine.succeed("test -f /var/lib/braid/acked-stats.json")
    acked = json.loads(machine.succeed("cat /var/lib/braid/acked-stats.json"))
    # Find the entry with missing_acked = true
    has_missing_acked = any(
        v.get("missing_acked", False) for v in acked.values()
    )
    assert has_missing_acked, f"Expected missing_acked=true in acked stats, got: {acked}"

with subtest("After ack: status has no ALERT"):
    output = machine.succeed("braid status")
    assert "ALERT" not in output, f"Expected no ALERT after ack, got: {output}"

with subtest("After ack: monitor exits 0"):
    machine.succeed("braid monitor")

machine.shutdown()
