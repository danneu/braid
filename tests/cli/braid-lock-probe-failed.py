# Test: braid-lock-probe-failed
#
# Intent: Verify `braid lock` uses the mounted `ProbeFailed` fallback when
#   per-device probing fails but FSID probing still succeeds.
#
# Why it exists: Real btrfs output can contain a non-`/dev/mapper/` device
#   path for braid's own mounted pool; this must not regress to a generic
#   abort or name-derived mapper cleanup.
#
# Scenario: A two-disk braid pool stays mounted, an operator manually adds a
#   raw spare device with `btrfs device add`, and an unverified braid-prefixed
#   mapper candidate is present. `braid lock` must unmount the pool, close the
#   UUID-verified member mappers, and skip the unverified candidate.

import shlex

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
pq = shlex.quote(passphrase)


def add_cmd(name):
    return (
        f"printf '%s\\n' {pq} | "
        "braid add "
        "--luks-format-arg=--pbkdf "
        "--luks-format-arg=pbkdf2 "
        "--luks-format-arg=--pbkdf-force-iterations "
        "--luks-format-arg=1000 "
        f"{name}=/dev/disk/by-id/virtio-{name} "
        "--passphrase-stdin --yes"
    )


with subtest("Build a mounted two-disk braid pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed("mountpoint -q /mnt/storage")
    machine.succeed("test -e /dev/mapper/braid-disk1")
    machine.succeed("test -e /dev/mapper/braid-disk2")
    pool_json = machine.succeed("cat /var/lib/braid/pool.json")
    assert "disk1" in pool_json, pool_json
    assert "disk2" in pool_json, pool_json
    assert "spare" not in pool_json, pool_json

with subtest("Add a raw btrfs device and an unverifiable mapper candidate"):
    machine.succeed("touch /dev/mapper/braid-BOGUS")
    machine.succeed("test -e /dev/mapper/braid-BOGUS")
    machine.succeed("btrfs device add -f /dev/disk/by-id/virtio-spare /mnt/storage")
    fi_show = machine.succeed("btrfs filesystem show /mnt/storage")
    assert "/dev/mapper/braid-disk1" in fi_show, fi_show
    assert "/dev/mapper/braid-disk2" in fi_show, fi_show
    assert "virtio-spare" in fi_show or "/dev/vd" in fi_show, fi_show
    pool_json = machine.succeed("cat /var/lib/braid/pool.json")
    assert "spare" not in pool_json, pool_json

with subtest("Dry-run previews UUID-scanned fallback without side effects"):
    status, output = machine.execute("braid lock --dry-run 2>&1")
    assert status == 0, "braid lock --dry-run failed: " + output
    assert "per-device probe failed (" in output, output
    assert "not a /dev/mapper/ path" in output, output
    assert "falling back to UUID-scanned mapper cleanup" in output, output
    assert "not btrfs" not in output, output
    assert "cannot probe pool" not in output, output
    machine.succeed("mountpoint -q /mnt/storage")
    machine.succeed("test -e /dev/mapper/braid-disk1")
    machine.succeed("test -e /dev/mapper/braid-disk2")
    machine.succeed("test -e /dev/mapper/braid-BOGUS")

with subtest("Real lock closes only UUID-verified member mappers"):
    status, output = machine.execute("braid lock 2>&1")
    assert status == 0, "braid lock failed: " + output
    assert "disk disk1: locked" in output, output
    assert "disk disk2: locked" in output, output
    assert "skipping mapper braid-BOGUS" in output, output
    assert "already closed" not in output, output
    machine.fail("mountpoint -q /mnt/storage")
    machine.fail("test -e /dev/mapper/braid-disk1")
    machine.fail("test -e /dev/mapper/braid-disk2")
    machine.succeed("test -e /dev/mapper/braid-BOGUS")

machine.shutdown()
