# Test: braid-alert service lifecycle and PC speaker plumbing
#
# Intent: Verify that the braid monitor timer and alert service units
#   exist, can be started/stopped, and that the PC speaker setup
#   (modprobe fallback, privilege drop, alertCommand privileges) works.
#
# Why it exists: beep silently fails on NixOS without pcspkr un-blacklisted,
#   the kernel module loaded, and proper evdev permissions. These tests prove
#   the plumbing works end-to-end so alerts actually fire.
#
# Scenario: NixOS machine with braid.monitor enabled and alertCommand set to
#   touch a root-owned file. The VM has no PC speaker hardware, so beep itself
#   silently fails — but we verify the surrounding machinery is correct.

start_all()
machine.wait_for_unit("multi-user.target")

with subtest("Monitor timer is active"):
    machine.succeed("systemctl is-active braid-monitor.timer")

with subtest("Alert service unit exists"):
    # The service should not be active by default (no alert yet),
    # but the unit file must be loadable.
    machine.succeed("systemctl cat braid-alert.service")

with subtest("Service script has modprobe fallback and setpriv wrapper"):
    # Verify the rendered service script includes the modprobe fallback
    # (for nixos-rebuild switch without reboot) and wraps beep with setpriv
    # (beep refuses to run as root). The VM lacks pcspkr hardware so we
    # can't test actual module loading, but we can verify the script shape.
    # systemctl cat shows the unit file; the actual script is in ExecStart=.
    exec_start = machine.succeed(
        "systemctl cat braid-alert.service | grep '^ExecStart=' | sed 's/ExecStart=//'"
    ).strip()
    script = machine.succeed(f"cat {exec_start}")
    assert "modprobe" in script and "pcspkr" in script, "must include modprobe pcspkr fallback"
    assert "setpriv" in script, "must use setpriv for beep"
    assert "reuid=nobody" in script, "setpriv must drop to nobody"
    assert "regid=beep" in script, "setpriv must drop to beep group"

with subtest("Privilege drop to beep group works"):
    # Prove the privilege drop mechanism works on this system — beep group
    # exists, nobody user exists, setpriv can drop to the right identity.
    machine.succeed("setpriv --reuid=nobody --regid=beep --groups=beep -- id -gn | grep beep")

with subtest("alertCommand runs as root"):
    # alertCommand must stay privileged — it may touch root-owned paths.
    # The service runs as root; only the beep call drops privileges.
    machine.succeed("rm -f /root/alert-fired")
    machine.succeed("systemctl start braid-alert.service")
    machine.wait_until_succeeds("test -f /root/alert-fired")
    machine.succeed("stat -c '%U' /root/alert-fired | grep root")
    machine.succeed("systemctl stop braid-alert.service")

with subtest("Alert service can be started and stopped"):
    machine.succeed("systemctl start braid-alert.service")
    machine.succeed("systemctl is-active braid-alert.service")
    machine.succeed("systemctl stop braid-alert.service")
    # After stop, service should be inactive
    machine.fail("systemctl is-active braid-alert.service")

machine.shutdown()
