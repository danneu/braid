# Test: braid-alert service lifecycle
#
# Intent: Verify that the braid monitor timer and alert service units
#   exist and can be started/stopped.
#
# Why it exists: If the systemd units are misconfigured or missing,
#   disk health alerts will never fire. This test catches unit definition
#   errors early.
#
# Scenario: NixOS machine with braid.monitor enabled. Check that the
#   timer is active and the alert service can cycle through start/stop.

start_all()
machine.wait_for_unit("multi-user.target")

with subtest("Monitor timer is active"):
    machine.succeed("systemctl is-active braid-monitor.timer")

with subtest("Alert service unit exists"):
    # The service should not be active by default (no alert yet),
    # but the unit file must be loadable.
    machine.succeed("systemctl cat braid-alert.service")

with subtest("Alert service can be started and stopped"):
    machine.succeed("systemctl start braid-alert.service")
    machine.succeed("systemctl is-active braid-alert.service")
    machine.succeed("systemctl stop braid-alert.service")
    # After stop, service should be inactive
    machine.fail("systemctl is-active braid-alert.service")

machine.shutdown()
