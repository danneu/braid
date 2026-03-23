# Test: braid-alert service with beep disabled
#
# Intent: Verify that braid.monitor.beep = false removes all PC speaker
#   plumbing while keeping alertCommand and the latched alert service working.
#
# Why it exists: beep = false is a new behavioral branch. Without this test,
#   the module could ship with stray PC speaker config, broken alertCommand,
#   or a non-functional alert service when beep is off.
#
# Scenario: Headless NAS with beep disabled and alertCommand set to touch a
#   file. Verify PC speaker plumbing is absent, alertCommand fires as root,
#   and the alert service latches active (RemainAfterExit) until stopped.

start_all()
machine.wait_for_unit("multi-user.target")

with subtest("Monitor timer is active"):
    machine.succeed("systemctl is-active braid-monitor.timer")

with subtest("Alert service unit exists"):
    machine.succeed("systemctl cat braid-alert.service")

with subtest("Service script omits beep plumbing"):
    # With beep disabled, the rendered script must not reference modprobe,
    # pcspkr, setpriv, or the beep binary.
    exec_start = machine.succeed(
        "systemctl cat braid-alert.service | grep '^ExecStart=' | sed 's/ExecStart=//'"
    ).strip()
    script = machine.succeed("cat %s" % exec_start)
    assert "modprobe" not in script, "script must not contain modprobe when beep is off"
    assert "pcspkr" not in script, "script must not contain pcspkr when beep is off"
    assert "setpriv" not in script, "script must not contain setpriv when beep is off"
    assert "/bin/beep" not in script, "script must not contain beep binary when beep is off"

with subtest("pcspkr not loaded at boot"):
    machine.fail("grep pcspkr /etc/modules-load.d/* 2>/dev/null")

with subtest("pcspkr blacklist not removed from modprobe config"):
    # The Ubuntu kmod blacklist includes pcspkr by default. When beep is off,
    # the overlay that removes that blacklist entry must NOT be applied.
    machine.succeed("grep pcspkr /etc/modprobe.d/*.conf")

with subtest("beep group absent"):
    machine.fail("getent group beep")

with subtest("alertCommand runs as root"):
    machine.succeed("rm -f /root/alert-fired")
    machine.succeed("systemctl start braid-alert.service")
    # RemainAfterExit oneshot — alertCommand runs during start, then exits.
    machine.wait_until_succeeds("test -f /root/alert-fired")
    machine.succeed("stat -c '%U' /root/alert-fired | grep root")

with subtest("Service latches active after exit (RemainAfterExit)"):
    machine.succeed("systemctl is-active braid-alert.service")

with subtest("Service can be stopped (simulates braid ack)"):
    machine.succeed("systemctl stop braid-alert.service")
    machine.fail("systemctl is-active braid-alert.service")

machine.shutdown()
