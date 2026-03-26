# Test: add-bootstrap
#
# Intent: Verify that braid add (bootstrap — first disk) creates the pool
# and the wrapper sets mount point permissions to root:storage 2770.
#
# Why it exists: braid add mounts from the Rust CLI, not through a systemd
# service. The wrapper-based permission fixup must cover this path. A
# regression would leave root:root 0755, blocking rsync/Samba/non-root writes.
#
# Scenario: First-time user runs braid add disk1. Pool is created, mounted,
# and the wrapper sets root:storage 2770 before returning.

start_all()
machine.wait_for_unit("multi-user.target", timeout=120)

with subtest("Bootstrap: braid add creates pool with correct permissions"):
    machine.succeed("echo -n 'testpassphrase' | braid add disk1=/dev/disk/by-id/virtio-disk1 --passphrase-stdin --yes")
    machine.succeed("mountpoint -q /mnt/storage")
    stat = machine.succeed("stat -c '%U:%G %a' /mnt/storage").strip()
    assert stat == "root:storage 2770", f"Expected root:storage 2770, got {stat}"

machine.shutdown()
