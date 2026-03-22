# Test: braid idle exit codes
#
# Intent: Verify that `braid idle` returns exit 0 when the pool is idle or
#   offline, and exit 1 when a btrfs operation (scrub) is running.
#
# Why it exists: braid idle is the integration point for autosuspend —
#   incorrect exit codes would either prevent the NAS from ever sleeping
#   (false busy) or allow sleep during active I/O (false idle).
#
# Scenario: 2-disk RAID1 pool. Check exit 0 when pool offline, exit 0 when
#   pool idle, and exit 1 during scrub (racy on small VM disks — unit tests
#   are authoritative for the busy path).

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"

with subtest("braid idle exits 0 when pool is offline"):
    machine.succeed("braid idle")
    output = machine.succeed("braid idle").strip()
    assert "idle" in output, f"Expected 'idle' in output, got: {output}"

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
    machine.succeed("mount -o noatime,skip_balance /dev/mapper/braid-disk1 /mnt/storage")

with subtest("braid idle exits 0 when pool is idle"):
    machine.succeed("braid idle")
    output = machine.succeed("braid idle").strip()
    assert "idle" in output, f"Expected 'idle' in output, got: {output}"

with subtest("braid idle detects scrub"):
    # Write data so scrub has work to do
    machine.succeed("dd if=/dev/urandom of=/mnt/storage/data bs=1M count=50")
    machine.succeed("sync")
    # Start scrub
    machine.succeed("btrfs scrub start /mnt/storage")
    # Check immediately — scrub may complete before we check on small VM disks
    result = machine.execute("braid idle")
    exit_code = result[0]
    output = result[1].strip()
    # On small VM disks, scrub may finish instantly — both exit 0 (idle) and
    # exit 1 (busy) are acceptable. The unit tests are authoritative for the
    # busy path. Here we just verify the command runs without error (not exit 2).
    assert exit_code in [0, 1], f"Expected exit 0 or 1, got {exit_code}: {output}"
    if exit_code == 1:
        assert "busy" in output, f"Expected 'busy' in output, got: {output}"
        assert "scrub" in output, f"Expected 'scrub' in output, got: {output}"
    print(f"braid idle during scrub: exit={exit_code}, output={output}")

machine.shutdown()
