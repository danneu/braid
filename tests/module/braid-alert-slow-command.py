# Test: braid-alert slow alertCommand
#
# Intent: Verify that a hung alertCommand is bounded and cannot hold the
#   audible beep hostage.
#
# Why it exists: `systemctl start` for a oneshot waits for ExecStart to finish,
#   and systemd gives oneshot units no default start timeout. The alert command
#   must be wrapped, and the beep unit must start in parallel with the
#   orchestrator instead of being ordered after it.
#
# Scenario: beep is enabled and alertCommand writes a marker, then sleeps
#   forever. The test starts braid-alert.service with --no-block, observes
#   braid-beep.service while the command is still blocked, then waits for
#   braid-alert.service to reach active/exited within the configured timeout.

start_all()
machine.wait_for_unit("multi-user.target")


def unit_script(unit):
    exec_start = machine.succeed(
        "systemctl cat {} | grep '^ExecStart=' | sed 's/ExecStart=//'".format(unit)
    ).strip()
    return machine.succeed("cat {}".format(exec_start))


with subtest("alert and advisory commands use the bounded wrapper"):
    alert_script = unit_script("braid-alert.service")
    advisory_script = unit_script("braid-alert-advisory.service")
    for unit_name, script in [
        ("braid-alert.service", alert_script),
        ("braid-alert-advisory.service", advisory_script),
    ]:
        assert "timeout -k 5s 10s" in script, (
            "{} must use the configured timeout wrapper:\n{}".format(
                unit_name, script
            )
        )
        assert "sleep infinity" in script, (
            "{} must run the configured alertCommand:\n{}".format(
                unit_name, script
            )
        )

with subtest("slow alertCommand does not delay the beep"):
    machine.succeed("rm -f /run/alert-command-started")
    machine.succeed("systemctl start --no-block braid-alert.service")
    machine.wait_until_succeeds("test -f /run/alert-command-started", timeout=30)
    assert (
        machine.succeed(
            "systemctl show braid-alert.service -p ActiveState --value"
        ).strip()
        == "activating"
    )
    machine.wait_until_succeeds("systemctl is-active braid-beep.service", timeout=5)
    active_state = machine.succeed(
        "systemctl show braid-alert.service -p ActiveState --value"
    ).strip()
    assert active_state == "activating", (
        "beep must start while alertCommand is still blocked, got " + active_state
    )

with subtest("timeout wrapper lets the alert latch form"):
    machine.wait_until_succeeds(
        '[ "$(systemctl show braid-alert.service -p ActiveState --value)" = active ]',
        timeout=30,
    )
    assert (
        machine.succeed(
            "systemctl show braid-alert.service -p SubState --value"
        ).strip()
        == "exited"
    )
    machine.succeed("systemctl stop braid-alert.service")
    machine.wait_until_succeeds(
        "! systemctl is-active --quiet braid-beep.service", timeout=30
    )

machine.shutdown()
