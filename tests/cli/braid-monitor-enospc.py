# Test: braid monitor - proactive ENOSPC risk (Warning tier)
#
# Intent: Verify the full proactive-capacity-alert lifecycle through the real
#   systemd path: a filling RAID1 pool crosses the ENOSPC threshold, monitor
#   exits 3 (Warning), the wrapper routes that to the non-beeping advisory
#   service, status shows the WARNING alert banner + enospc_risk cause, and ack snoozes
#   the reminder (clears the latch + stops the advisory unit) without resolving --
#   status keeps showing the live advisory, the monitor stays quiet within the
#   snooze window, re-alerts once the reminder interval elapses, and goes quiet
#   again after a re-ack. A degraded pool raises MissingDevice (Critical) but
#   never EnospcRisk.
#
# Why it exists: Unit tests cover the state machine in isolation. Only a VM
#   check proves the exit-3 wrapper routing, the advisory systemd unit, the real
#   `systemctl stop` on ack, and degraded-pool precedence.
#
# Scenario: 2-disk RAID1 pool (disk1, disk2) pre-created by the initrd fixture,
#   unlocked via braid-pool.target, filled until at risk, acknowledged, then
#   remounted degraded to prove MissingDevice wins over EnospcRisk. See
#   braid-monitor-enospc-geometry for keyed-baseline invalidation after a
#   same-devid geometry change.

import json

start_all()
machine.wait_for_unit("multi-user.target", timeout=120)

passphrase = "testpassphrase"

with subtest("Monitor timer is active, then stopped for deterministic driving"):
    machine.succeed("systemctl is-active braid-monitor.timer")
    # Stop the timer so a 5-minute tick cannot race the manual monitor runs and
    # mutate the latch / advisory mid-assertion.
    machine.succeed("systemctl stop braid-monitor.timer")

with subtest("Unlock pool via braid-pool.target"):
    machine.succeed("systemctl start braid-pool.target")
    machine.succeed("mountpoint -q /mnt/storage")

with subtest("Healthy empty pool: monitor exits 0"):
    rc = machine.succeed("set +e; braid monitor; echo $?").strip().splitlines()[-1]
    assert rc == "0", f"Expected exit 0 on a healthy pool, got {rc}"

with subtest("Fill the pool below the ENOSPC threshold"):
    # RAID1 mirrors each write to both devices, so writing data drops both
    # devices' unallocated space below the per-device threshold
    # (min(1 GiB, 10% of total) -> ~100 MiB for this 2x512 MiB pool). Write
    # 50 MiB files until braid's own predicate reports at-risk, or the pool
    # fills. Using `braid status` as the gate keeps the fill amount robust to
    # LUKS/metadata overhead.
    for i in range(14):
        machine.execute(
            f"dd if=/dev/zero of=/mnt/storage/fill{i} bs=1M count=50 2>/dev/null"
        )
        machine.execute("sync")
        if "ENOSPC risk" in machine.succeed("braid status"):
            break
    print(machine.succeed("btrfs device usage --raw /mnt/storage"))
    assert "ENOSPC risk" in machine.succeed("braid status"), (
        "fill did not cross the ENOSPC threshold"
    )

with subtest("At-risk pool: braid monitor exits 3 (Warning, not the beeping exit 1)"):
    rc = machine.succeed("set +e; braid monitor; echo $?").strip().splitlines()[-1]
    assert rc == "3", f"Expected exit 3 (Warning), got {rc}"
    machine.succeed("test -f /var/lib/braid/alert-latch.json")

with subtest("Wrapper routes exit 3 to the non-beeping advisory service"):
    machine.succeed("rm -f /root/alert-fired")
    machine.succeed("systemctl start braid-monitor.service")
    # The advisory ran alertCommand; the beeper did NOT start.
    machine.wait_until_succeeds("test -f /root/alert-fired")
    machine.succeed("systemctl is-active braid-alert-advisory.service")
    machine.fail("systemctl is-active braid-alert.service")

with subtest("Status shows the enospc_risk cause and the WARNING (not Critical) alert banner"):
    report = json.loads(machine.succeed("braid status --json"))
    assert report["alert_active"] is True, f"expected alert_active, got {report}"
    cause_types = [c["type"] for c in report["alert_causes"]]
    assert "enospc_risk" in cause_types, f"expected enospc_risk cause, got {cause_types}"
    human = machine.succeed("braid status")
    assert "WARNING alert -- capacity risk detected" in human, (
        f"expected WARNING alert banner, got:\n{human}"
    )
    assert "CRITICAL alert" not in human, (
        f"a Warning must not render the critical banner:\n{human}"
    )
    assert "ENOSPC risk: pool is one disk-loss" in human, (
        f"expected the ENOSPC cause line, got:\n{human}"
    )

with subtest("Ack snoozes: clears the latch, writes the snooze marker, stops the advisory unit"):
    machine.succeed("braid ack")
    machine.fail("test -f /var/lib/braid/alert-latch.json")
    machine.succeed("test -f /var/lib/braid/enospc-ack.json")
    machine.fail("systemctl is-active braid-alert-advisory.service")

with subtest("Acked-but-still-at-risk pool: follow-up monitor exits 0 (within the snooze window)"):
    rc = machine.succeed("set +e; braid monitor; echo $?").strip().splitlines()[-1]
    assert rc == "0", f"Expected exit 0 (suppressed within the snooze window), got {rc}"
    machine.fail("test -f /var/lib/braid/alert-latch.json")

with subtest("Ack snoozes but does not resolve -- status still shows the live advisory"):
    # The marker exists (the real ack above wrote it). status recomputes risk live
    # from the pool, independent of the marker, so the advisory must persist.
    assert "ENOSPC risk" in machine.succeed("braid status"), (
        "ack snoozes the reminder; it must not resolve the live status advisory"
    )

with subtest("Reminder elapses -> monitor re-alerts (exit 3)"):
    # Rewrite the snooze deadline into the past (preserving pool_key) to simulate
    # the reminder interval elapsing. The monitor timer is stopped, so nothing
    # races this edit.
    machine.succeed(
        "tmp=$(mktemp); jq '.snoozed_until = 1' /var/lib/braid/enospc-ack.json > \"$tmp\" "
        '&& mv "$tmp" /var/lib/braid/enospc-ack.json'
    )
    rc = machine.succeed("set +e; braid monitor; echo $?").strip().splitlines()[-1]
    assert rc == "3", f"Expected exit 3 (reminder re-fires after the interval), got {rc}"
    machine.succeed("test -f /var/lib/braid/alert-latch.json")

with subtest("Re-ack snoozes for another interval -> exit 0"):
    machine.succeed("braid ack")
    rc = machine.succeed("set +e; braid monitor; echo $?").strip().splitlines()[-1]
    assert rc == "0", f"Expected exit 0 (re-ack re-opened the snooze window), got {rc}"
    # Prove the re-ack stamped a fresh future deadline, not a vacuous pass.
    now = int(machine.succeed("date +%s").strip())
    deadline = int(
        machine.succeed("jq '.snoozed_until' /var/lib/braid/enospc-ack.json").strip()
    )
    assert deadline > now, f"re-ack must stamp a future deadline; got {deadline} <= {now}"

# Note: baseline-invalidation-on-topology-change (F1) is covered by the unit
# test cmd_monitor_stale_baseline_key_mismatch_fires_and_clears across all three
# mismatch axes. The same-devid `device_size` axis is also driven end-to-end by
# braid-monitor-enospc-geometry via `braid replace`. `braid add` remains
# unusable as the end-to-end vehicle here because its RAID1 balance either
# ENOSPCs on a deliberately-near-full pool or relieves the risk on a non-full
# one.

with subtest("Degraded pool raises MissingDevice (Critical) but never EnospcRisk"):
    # The latch is clean (ack above cleared it). Fail a disk and remount
    # degraded; monitor must skip ENOSPC entirely and raise only MissingDevice.
    machine.succeed("test -f /var/lib/braid/enospc-ack.json")
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup close braid-disk2")
    machine.succeed("mount -o degraded /dev/mapper/braid-disk1 /mnt/storage")
    rc = machine.succeed("set +e; braid monitor; echo $?").strip().splitlines()[-1]
    assert rc == "1", f"Expected exit 1 (Critical MissingDevice) on degraded pool, got {rc}"
    report = json.loads(machine.succeed("braid status --json"))
    cause_types = [c["type"] for c in report["alert_causes"]]
    assert "missing_device" in cause_types, f"expected missing_device, got {cause_types}"
    assert "enospc_risk" not in cause_types, (
        f"a degraded pool must not raise EnospcRisk, got {cause_types}"
    )
    # The skip-before-the-state-machine guarantee: the baseline survives a
    # degraded cycle untouched.
    machine.succeed("test -f /var/lib/braid/enospc-ack.json")

machine.shutdown()
