# Test: monitor-lifecycle
#
# Intent: Verify the end-to-end systemd monitoring chain — timer activation,
#   ConditionPathIsMountPoint gate on braid-monitor.service, monitor-triggered
#   alert service activation, and braid ack clearing through systemd.
#
# Why it exists: Existing tests verify CLI-level monitor/ack behavior
#   (braid-monitor, braid-smartd-alert) and alert service unit properties
#   (braid-alert, braid-alert-no-beep), but no test exercises the systemd
#   integration: braid-monitor.service starting braid-alert.service on a
#   degraded pool, condition-gating preventing alert side effects when unmounted,
#   or braid ack stopping the alert service through the real systemd path.
#
# Scenario: 3-disk RAID1 pool pre-created by initrd fixture. Pool is unlocked
#   via braid-pool.target. Monitor runs healthy. One LUKS mapper is closed to
#   simulate drive failure. Monitor detects degraded state and starts alert
#   service. braid ack clears alert. Pool is locked; monitor produces no
#   alert side effects while unmounted.

start_all()
machine.wait_for_unit("multi-user.target", timeout=120)

passphrase = "testpassphrase"

# --- Subtest 1: Timer active at boot ---

with subtest("Monitor timer is active at boot"):
    machine.succeed("systemctl is-active braid-monitor.timer")

# --- Subtest 2: Monitor service carries mount-point gate ---

with subtest("braid-monitor.service carries the statx mount-point gate"):
    # Regression tripwire. The gate is a statx(STATX_ATTR_MOUNT_ROOT) check,
    # independent of the /proc/self/mountinfo parse `braid monitor` fails
    # closed on, so it skips only a confirmed-offline pool and never masks the
    # mounted-but-anomalous beep (see ADR 018). Subtests 3/9 below pass with or
    # without the gate (offline -> exit 0 -> no beep), so without this
    # assertion the gate could be deleted silently. Mirrors auto-scrub.py.
    unit = machine.succeed("systemctl cat braid-monitor.service")
    assert "ConditionPathIsMountPoint=/mnt/storage" in unit, (
        "braid-monitor.service must carry ConditionPathIsMountPoint; got:\n"
        + unit
    )

# --- Subtest 3: No alert side effects before mount ---

with subtest("No alert side effects before pool mount"):
    # Pool is not yet mounted. ConditionPathIsMountPoint gates the
    # service — systemd skips it cleanly (exit 0, no dependency failure).
    machine.succeed("rm -f /root/alert-fired")
    machine.succeed("systemctl start braid-monitor.service")
    machine.fail("systemctl is-active braid-alert.service")
    machine.fail("test -f /root/alert-fired")

# --- Subtest 4: Unlock pool ---

with subtest("Unlock pool via braid-pool.target"):
    machine.succeed("systemctl start braid-pool.target")
    machine.succeed("mountpoint -q /mnt/storage")
    machine.succeed("systemctl is-active braid-online.service")

# --- Subtest 5: Healthy monitor run produces no alert ---

with subtest("Healthy pool: monitor runs without triggering alert"):
    machine.succeed("rm -f /root/alert-fired")
    machine.succeed("systemctl start braid-monitor.service")
    machine.fail("systemctl is-active braid-alert.service")
    machine.fail("test -f /root/alert-fired")

# --- Subtest 6: Degrade pool ---

with subtest("Degrade pool by closing one LUKS mapper"):
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup close braid-disk3")
    machine.succeed(
        "mount -o degraded /dev/mapper/braid-disk1 /mnt/storage"
    )
    # Wait for systemd to detect the mount and activate the unit.
    machine.wait_until_succeeds("systemctl is-active mnt-storage.mount")

# --- Subtest 7: Monitor triggers alert on degraded pool ---

with subtest("Degraded pool: monitor triggers braid-alert.service"):
    machine.succeed("rm -f /root/alert-fired")
    machine.succeed("systemctl start braid-monitor.service")
    # braid-monitor.service always exits 0. When braid monitor returns
    # exit 1, the service script starts braid-alert.service.
    machine.succeed("systemctl is-active braid-alert.service")
    machine.succeed("test -f /root/alert-fired")

# --- Subtest 8: Ack clears alert via systemd ---

with subtest("braid ack clears alert and stops alert service"):
    machine.succeed("braid ack")
    machine.fail("systemctl is-active braid-alert.service")
    machine.fail("test -f /var/lib/braid/alert-latch.json")

# --- Subtest 9: No alert side effects after unmount ---

with subtest("No alert side effects after pool unmount"):
    machine.succeed("rm -f /root/alert-fired")
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup close braid-disk1")
    machine.succeed("cryptsetup close braid-disk2")
    machine.fail("mountpoint -q /mnt/storage")
    # ConditionPathIsMountPoint: clean skip, not a dependency failure.
    machine.succeed("systemctl start braid-monitor.service")
    machine.fail("systemctl is-active braid-alert.service")
    machine.fail("test -f /root/alert-fired")

machine.shutdown()
