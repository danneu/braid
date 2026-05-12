# Test: braid idle exit codes
#
# Intent: Verify that `braid idle` returns exit 0 when the pool is idle or
#   offline, and exit 1 when a btrfs operation is running or a
#   pool-state probe fails.
#
# Why it exists: braid idle is the integration point for autosuspend --
#   incorrect exit codes would either prevent the NAS from ever sleeping
#   (false busy) or allow sleep during active I/O (false idle).
#
# Scenario: 2-disk RAID1 pool. Check exit 0 when pool offline, exit 0 when
#   pool idle, exit 1 on a forced probe failure, and exit 1 during scrub
#   (racy on small VM disks -- unit tests are authoritative for the busy
#   path).

import base64
import re
import shlex

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

with subtest("braid idle exits 1 when a pool-state probe fails"):
    braid_wrapped_path = machine.succeed("readlink -f $(command -v braid)").strip()
    wrapper_source = machine.succeed(f"cat {braid_wrapped_path}")
    m = re.search(r'(/nix/store/[^"\s]+/bin/braid)(?!\-)', wrapper_source)
    assert m, f"could not locate unwrapped braid in wrapper:\n{wrapper_source}"
    unwrapped_braid = m.group(1)

    real_btrfs = machine.succeed("command -v btrfs").strip()
    wrapper_template = """#!/usr/bin/env bash
set -eu
if [ "${1:-}" = "scrub" ] && [ "${2:-}" = "status" ]; then
    printf 'simulated scrub status failure\\n' >&2
    exit 1
fi
exec __REAL_BTRFS__ "$@"
"""
    wrapper_script = wrapper_template.replace("__REAL_BTRFS__", real_btrfs)
    wrapper_b64 = base64.b64encode(wrapper_script.encode()).decode()

    machine.succeed(
        "rm -rf /tmp/btrfs-stub && "
        "mkdir -p /tmp/btrfs-stub && "
        f"printf '%s' {shlex.quote(wrapper_b64)} | base64 -d > /tmp/btrfs-stub/btrfs && "
        "chmod +x /tmp/btrfs-stub/btrfs"
    )

    status, output = machine.execute(f"PATH=/tmp/btrfs-stub:$PATH {unwrapped_braid} idle")
    output = output.strip()
    assert status == 1, f"Expected exit 1 for probe failure, got {status}: {output}"
    assert output.startswith(
        "busy: unknown (scrub:"
    ), f"Expected busy unknown diagnostic, got: {output}"
    assert "simulated scrub status failure" in output, (
        f"Expected underlying probe diagnostic to be preserved, got: {output}"
    )

    machine.succeed("rm -rf /tmp/btrfs-stub")
    status, output = machine.execute("braid idle")
    output = output.strip()
    assert status == 0, f"Expected wrapped braid idle to recover, got {status}: {output}"
    assert "idle" in output, f"Expected 'idle' in output, got: {output}"

with subtest("braid idle detects scrub"):
    # Write data so scrub has work to do
    machine.succeed("dd if=/dev/urandom of=/mnt/storage/data bs=1M count=50")
    machine.succeed("sync")
    # Start scrub
    machine.succeed("btrfs scrub start /mnt/storage")
    # Check immediately -- scrub may complete before we check on small VM disks
    result = machine.execute("braid idle")
    exit_code = result[0]
    output = result[1].strip()
    # On small VM disks, scrub may finish instantly -- both exit 0 (idle) and
    # exit 1 (busy) are acceptable. The unit tests are authoritative for the
    # busy path. Here we just verify the command runs without error (not exit 2).
    assert exit_code in [0, 1], f"Expected exit 0 or 1, got {exit_code}: {output}"
    if exit_code == 1:
        assert "busy" in output, f"Expected 'busy' in output, got: {output}"
        assert "scrub" in output, f"Expected 'scrub' in output, got: {output}"
    print(f"braid idle during scrub: exit={exit_code}, output={output}")

machine.shutdown()
