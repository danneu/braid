# Test: smartd alert lifecycle
#
# Intent: Verify smartd-triggered alerts appear in `braid status` and
#   clear with `braid ack`.
#
# Why it exists: smartd alerts use a flag file as the bridge into braid's
#   alert model. Without this test, a broken flag file path or ack cleanup
#   would go unnoticed.
#
# Scenario: Healthy 2-disk RAID1 pool. Simulate smartd alert by touching
#   the flag file. Verify monitor detects it, status shows SMART warning,
#   ack clears the flag.

import json

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"

# --- Setup: create 2-disk RAID1 pool ---
with subtest("Create 2-disk RAID1 pool"):
    for d in ["disk1", "disk2"]:
        machine.succeed(
            f"echo -n '{passphrase}' | cryptsetup luksFormat --batch-mode --key-file=- --pbkdf pbkdf2 --pbkdf-force-iterations 1000 /dev/disk/by-id/virtio-{d}"
        )
        machine.succeed(
            f"echo -n '{passphrase}' | cryptsetup open --type luks --key-file=- /dev/disk/by-id/virtio-{d} braid-{d}"
        )

    machine.succeed(
        "mkfs.btrfs -f -d raid1 -m raid1 /dev/mapper/braid-disk1 /dev/mapper/braid-disk2"
    )
    machine.succeed("mkdir -p /mnt/storage")
    machine.succeed("mount /dev/mapper/braid-disk1 /mnt/storage")
    machine.succeed("mkdir -p /var/lib/braid")

with subtest("Healthy pool: no ALERT"):
    output = machine.succeed("braid status")
    assert "ALERT" not in output, f"Expected no ALERT, got: {output}"

with subtest("Simulate smartd alert"):
    machine.succeed("touch /var/lib/braid/smartd-alert")

with subtest("After smartd alert: monitor exits 1"):
    machine.fail("braid monitor")

with subtest("After smartd alert: status shows SMART warning"):
    output = machine.succeed("braid status")
    assert "ALERT" in output, f"Expected ALERT, got: {output}"
    assert "SMART" in output, f"Expected SMART cause, got: {output}"

with subtest("Ack clears smartd alert"):
    machine.succeed("braid ack")
    # Flag file should be removed
    machine.fail("test -f /var/lib/braid/smartd-alert")

with subtest("After ack: no ALERT"):
    output = machine.succeed("braid status")
    assert "ALERT" not in output, f"Expected no ALERT after ack, got: {output}"

with subtest("After ack: monitor exits 0"):
    machine.succeed("braid monitor")

machine.shutdown()
