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

with subtest("Degraded pool: monitor exit code is exactly 1"):
    rc = machine.succeed("set +e; braid monitor; echo $?").strip().splitlines()[-1]
    assert rc == "1", f"Expected exit 1, got {rc}"

with subtest("Degraded pool: latch file created"):
    machine.succeed("test -f /var/lib/braid/alert-latch.json")

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

with subtest("Ack removes latch file"):
    machine.fail("test -f /var/lib/braid/alert-latch.json")

with subtest("After ack: status has no ALERT"):
    output = machine.succeed("braid status")
    assert "ALERT" not in output, f"Expected no ALERT after ack, got: {output}"

with subtest("After ack: monitor exits 0"):
    machine.succeed("braid monitor")

# Intent: A corrupt /var/lib/braid/alert-latch.json must surface as a loud
#   alert rather than silently rebuilding into an empty latch.
# Why it exists: The prior load_alert_latch returned None on parse failure,
#   conflating "absent" with "corrupt". cmd_monitor would then merge live
#   causes onto an empty slate and overwrite the corrupt file -- silently
#   dropping previously-latched-but-now-cleared causes and violating the
#   "latched until ack" invariant. status would also report "no alert" so
#   the operator never noticed.
# Scenario: external tampering or filesystem damage corrupts the latch
#   while the pool is mounted and healthy. monitor must exit 1 (not 0,
#   not 2), the corrupt bytes must be preserved in the .corrupt sidecar,
#   status must surface the corruption, and ack must clear both files.
with subtest("Corrupt latch (mounted): monitor surfaces and quarantines"):
    # Pool is currently mounted and healthy; no real alert exists.
    machine.succeed("printf 'not json' > /var/lib/braid/alert-latch.json")
    rc = machine.succeed("set +e; braid monitor; echo $?").strip().splitlines()[-1]
    assert rc == "1", f"Expected monitor exit 1 on corrupt latch, got {rc}"
    # Corrupt bytes preserved in sidecar
    machine.succeed("test -f /var/lib/braid/alert-latch.json.corrupt")
    sidecar = machine.succeed("cat /var/lib/braid/alert-latch.json.corrupt")
    assert sidecar == "not json", f"Expected sidecar to hold original bytes, got: {sidecar!r}"
    # status exits 0 (status never returns non-zero on alerts) but surfaces it
    json_output = machine.succeed("braid status --json")
    report = json.loads(json_output)
    assert report["alert_active"] == True, f"Expected alert_active=true, got: {report}"
    cause_types = [c["type"] for c in report["alert_causes"]]
    assert "computation_error" in cause_types, (
        f"Expected computation_error cause, got: {cause_types}"
    )
    ce_details = [c.get("detail", "") for c in report["alert_causes"] if c["type"] == "computation_error"]
    assert any("alert latch" in d for d in ce_details), (
        f"Expected ComputationError detail to mention 'alert latch', got: {ce_details}"
    )
    # ack clears both the live latch and the .corrupt sidecar
    machine.succeed("braid ack")
    machine.fail("test -f /var/lib/braid/alert-latch.json")
    machine.fail("test -f /var/lib/braid/alert-latch.json.corrupt")

with subtest("Btrfs alert latched after pool offline"):
    # Pool is still degraded. Remove acked state to re-trigger alert.
    machine.succeed("rm -f /var/lib/braid/acked-stats.json")
    rc = machine.succeed("set +e; braid monitor; echo $?").strip().splitlines()[-1]
    assert rc == "1", f"Expected exit 1, got {rc}"
    machine.succeed("test -f /var/lib/braid/alert-latch.json")
    # Now lock the pool
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup close braid-disk1")
    machine.succeed("cryptsetup close braid-disk3")
    # Status should still show the latched alert
    output = machine.succeed("braid status")
    assert "ALERT" in output, f"Expected ALERT in offline status, got: {output}"
    # Offline ack should succeed
    machine.succeed("braid ack")
    machine.fail("test -f /var/lib/braid/alert-latch.json")

# Intent: A corrupt latch must be ack-able even with the pool offline.
# Why it exists: If `latch_count = 0` is set on parse failure (the naive
#   fix) and smartd is inactive, ack_offline gates on
#   `has_alert = latch_count > 0 || smartd_active` and returns
#   PoolNotMounted, leaving the corrupt file on disk forever. The user
#   would have no way to clear it without remounting.
# Scenario: pool is offline (already the case at this point in the
#   script), and the latch on disk is unparseable. `braid ack` must
#   succeed and remove both the live latch and the .corrupt sidecar.
with subtest("Corrupt latch (offline): ack clears it without PoolNotMounted"):
    # Pool is currently offline (cryptsetup close was called above).
    # Both alert-latch.json and alert-latch.json.corrupt should be absent
    # at this point; create just the live (corrupt) file.
    machine.fail("test -f /var/lib/braid/alert-latch.json")
    machine.succeed("printf 'not json' > /var/lib/braid/alert-latch.json")
    rc = machine.succeed("set +e; braid ack; echo $?").strip().splitlines()[-1]
    assert rc == "0", f"Expected ack exit 0 on offline corrupt latch, got {rc}"
    machine.fail("test -f /var/lib/braid/alert-latch.json")
    machine.fail("test -f /var/lib/braid/alert-latch.json.corrupt")

machine.shutdown()
