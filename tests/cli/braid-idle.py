# Test: braid idle exit codes
#
# Intent: Verify that `braid idle` returns exit 0 when the pool is idle or
#   offline, exit 1 when a btrfs operation is running or a pool-state probe
#   fails, exit 1 at the non-root gate, and exit 2 when config loading fails.
#
# Why it exists: braid idle is the integration point for autosuspend --
#   incorrect exit codes would either prevent the NAS from ever sleeping
#   (false busy) or allow sleep during active I/O (false idle).
#
# Scenario: 2-disk RAID1 pool. Check exit 0 when pool offline, exit 2 for
#   unreadable or unparseable config, exit 1 before config/probes when run
#   without root, exit 0 when pool idle, exit 1 on a forced probe failure, and
#   exit 1 during a live scrub held running with dm-delay read throttling.

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

with subtest("braid idle exits 2 on config-load failure (setup error, not exit 1)"):
    # Exit 2 is the documented "config could not be read" contract (idle.md, ADR 016).
    # The path-substring check proves config_read ran: Cli::parse() runs before the
    # config load and Clap usage errors also exit 2, so the exit code alone is not
    # proof (the injected path only appears via ConfigError::{Read,Parse} Display).
    machine.succeed("echo 'not json {{{' > /tmp/bad.json")
    status, output = machine.execute("braid idle --config /tmp/bad.json 2>&1")
    assert status == 2, f"unparseable config must exit 2 (not 1), got {status}: {output}"
    assert "/tmp/bad.json" in output, f"exit 2 must be config-load (not clap usage), got: {output}"
    status, output = machine.execute("braid idle --config /tmp/nonexistent.json 2>&1")
    assert status == 2, f"missing config must exit 2 (not 1), got {status}: {output}"
    assert "/tmp/nonexistent.json" in output, f"exit 2 must be config-load (not clap usage), got: {output}"

with subtest("braid idle exits 1 at the non-root gate before config/probes"):
    machine.succeed("rm -f /tmp/idle.stdout /tmp/idle.stderr")
    status, output = machine.execute(
        "runuser -u nobody -- braid idle >/tmp/idle.stdout 2>/tmp/idle.stderr"
    )
    stdout = machine.succeed("cat /tmp/idle.stdout")
    stderr = machine.succeed("cat /tmp/idle.stderr")

    assert status == 1, f"non-root braid idle must exit 1, got {status}: {output}"
    assert "error: braid must be run as root" in stderr, (
        f"root-gate diagnostic must be on stderr, got stderr={stderr!r}"
    )
    assert stdout == "", f"root gate must not emit stdout, got: {stdout!r}"
    assert "idle:" not in stdout, f"root gate must not classify idle, got: {stdout!r}"
    assert "busy:" not in stdout, f"root gate must not classify busy, got: {stdout!r}"

    machine.succeed("rm -f /tmp/idle.stdout /tmp/idle.stderr")

with subtest("Create 2-disk RAID1 pool"):
    for d in ["disk1", "disk2"]:
        dm_delay_create(machine, d)
        by_id = f"/dev/disk/by-id/braid-test-{d}-delay"
        machine.succeed(
            f"echo -n '{passphrase}' | cryptsetup luksFormat --batch-mode --key-file=- --pbkdf pbkdf2 --pbkdf-force-iterations 1000 {by_id}"
        )
        machine.succeed(
            f"echo -n '{passphrase}' | cryptsetup open --type luks --key-file=- {by_id} braid-{d}"
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

with subtest("braid idle reports busy while a scrub is genuinely running"):
    machine.succeed("dd if=/dev/urandom of=/mnt/storage/data bs=1M count=64 conv=fsync")
    machine.succeed("sync")
    # Slow scrub reads enough that the scrub cannot reach a terminal state before
    # we sample it. `btrfs scrub status` only prints a `Status:` line once the
    # scrub daemon surfaces a progress record on its ~5s cycle, so the wait below
    # is mandatory. A finite delay keeps status/ioctl queries and `braid idle`
    # responsive while widening the post-wait running window.
    dm_delay_activate(machine, ["disk1", "disk2"], read_delay_ms=1000)
    # Redirect the scrub daemon's stdio off the driver pipe.
    machine.succeed("btrfs scrub start /mnt/storage > /dev/null 2>&1")
    machine.wait_until_succeeds(
        "btrfs scrub status --raw /mnt/storage | "
        "grep -Eq 'Status:[[:space:]]+running'",
        timeout=30,
    )

    status, output = machine.execute("braid idle")
    output = output.strip()
    assert status == 1, f"running scrub must make braid idle exit 1, got {status}: {output}"
    assert output.startswith("busy: scrub running"), (
        f"expected 'busy: scrub running', got: {output}"
    )

    # Release the read throttle first so the cancel ioctl cannot block behind a
    # delayed scrub read. Once drained the scrub may auto-finish before cancel
    # lands, so tolerate that and accept "finished" as a terminal state.
    dm_delay_deactivate(machine, ["disk1", "disk2"])
    machine.succeed("btrfs scrub cancel /mnt/storage || true")
    machine.wait_until_succeeds(
        "btrfs scrub status --raw /mnt/storage | "
        "grep -Eq 'Status:[[:space:]]+(aborted|interrupted|finished)'",
        timeout=30,
    )

machine.shutdown()
