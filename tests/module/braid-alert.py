# Test: braid-alert service lifecycle and PC speaker plumbing
#
# Intent: Verify that the monitor timer and alert units exist, the Critical
#   alert path starts a latched orchestrator plus a hardened beep loop, and the
#   PC speaker setup remains usable under the unit sandbox.
#
# Why it exists: beep silently fails on NixOS without pcspkr un-blacklisted,
#   the kernel module loaded, evdev permissions, and the setpriv capability
#   set. These tests prove the split units preserve that plumbing.
#
# Scenario: NixOS machine with braid.monitor enabled and alertCommand set to
#   touch a root-owned file. The VM has no PC speaker hardware, so beep itself
#   silently fails, but the surrounding lifecycle and privilege model are real.

start_all()
machine.wait_for_unit("multi-user.target")


def show(unit, prop):
    return machine.succeed(
        "systemctl show {} -p {} --value".format(unit, prop)
    ).strip()


def unit_script(unit):
    exec_start = machine.succeed(
        "systemctl cat {} | grep '^ExecStart=' | sed 's/ExecStart=//'".format(unit)
    ).strip()
    return machine.succeed("cat {}".format(exec_start))


def wait_inactive(unit):
    machine.wait_until_succeeds(
        "! systemctl is-active --quiet {}".format(unit), timeout=30
    )


def beep_caps():
    return set(show("braid-beep.service", "CapabilityBoundingSet").lower().split())


with subtest("Monitor timer is active"):
    machine.succeed("systemctl is-active braid-monitor.timer")

with subtest("Alert and beep units exist"):
    machine.succeed("systemctl cat braid-alert.service")
    machine.succeed("systemctl cat braid-beep.service")

with subtest("Alert orchestrator is a latched oneshot without the beep loop"):
    unit = machine.succeed("systemctl cat braid-alert.service")
    script = unit_script("braid-alert.service")
    assert show("braid-alert.service", "Type") == "oneshot"
    assert show("braid-alert.service", "RemainAfterExit") == "yes"
    assert "braid-beep.service" in unit, (
        "alert service must pull in braid-beep.service:\n" + unit
    )
    assert "braid-beep-probe" not in script, (
        "alert script must not contain the persistent beep loop:\n" + script
    )
    assert "delay=5" not in script, (
        "alert script must not contain the backoff loop:\n" + script
    )
    assert "modprobe" not in script, (
        "module loading stays isolated in braid-pcspkr-load.service:\n" + script
    )
    assert "timeout -k 5s 60s" in script, (
        "alertCommand must be bounded by the default timeout:\n" + script
    )

with subtest("Beep unit uses the canonical wrapper and exponential backoff"):
    script = unit_script("braid-beep.service")
    assert "braid-beep-probe" in script, (
        "beep script must reference the canonical braid-beep-probe wrapper:\n"
        + script
    )
    assert "delay=5" in script, f"beep script must initialize delay=5:\n{script}"
    assert "max_delay=900" in script, (
        f"beep script must cap delay at 900s:\n{script}"
    )
    assert "delay * 2" in script, (
        f"beep script must double the delay each iter:\n{script}"
    )
    assert "$max_delay" in script, (
        f"beep script must clamp to max_delay:\n{script}"
    )

    wrapper_path = machine.succeed(
        "grep -oE '/nix/store/[^[:space:]]*braid-beep-probe' "
        "$(systemctl cat braid-beep.service | grep '^ExecStart=' | sed 's/ExecStart=//') "
        "| head -1"
    ).strip()
    assert wrapper_path.endswith("braid-beep-probe"), (
        f"could not extract wrapper path from beep script:\n{script}"
    )
    wrapper_body = machine.succeed(f"cat {wrapper_path}")
    assert "setpriv" in wrapper_body, (
        f"wrapper must use setpriv for beep:\n{wrapper_body}"
    )
    assert "reuid=nobody" in wrapper_body, (
        f"wrapper must drop to nobody:\n{wrapper_body}"
    )
    assert "regid=beep" in wrapper_body, (
        f"wrapper must drop to beep group:\n{wrapper_body}"
    )

with subtest("pcspkr loader is pulled by each alert start"):
    alert_unit = machine.succeed("systemctl cat braid-alert.service")
    loader_unit = machine.succeed("systemctl cat braid-pcspkr-load.service")
    assert "Wants=braid-pcspkr-load.service" in alert_unit, (
        "alert service must want the pcspkr loader:\n" + alert_unit
    )
    assert "After=braid-pcspkr-load.service" in alert_unit, (
        "alert service must order after the pcspkr loader:\n" + alert_unit
    )
    loader_caps = show("braid-pcspkr-load.service", "CapabilityBoundingSet").lower()
    assert "cap_sys_module" in loader_caps, (
        "loader must keep module-load capability, got: " + loader_caps
    )
    assert show("braid-pcspkr-load.service", "ProtectKernelModules") == "no"
    assert show("braid-pcspkr-load.service", "PrivateNetwork") == "yes"
    assert "RemainAfterExit=" not in loader_unit, (
        "loader must be re-runnable, not active-after-exit:\n" + loader_unit
    )

    machine.succeed("systemctl reset-failed braid-pcspkr-load.service")
    before = show("braid-pcspkr-load.service", "ExecMainStartTimestampMonotonic")
    machine.succeed("rm -f /root/alert-fired")
    machine.succeed("systemctl start braid-alert.service")
    machine.wait_until_succeeds("test -f /root/alert-fired")
    first = show("braid-pcspkr-load.service", "ExecMainStartTimestampMonotonic")
    assert first not in ["", "0"], "loader must have a first start timestamp"
    assert first != before, "alert start must run the pcspkr loader"
    machine.succeed("systemctl stop braid-alert.service")
    wait_inactive("braid-beep.service")

    machine.succeed("rm -f /root/alert-fired")
    machine.succeed("systemctl start braid-alert.service")
    machine.wait_until_succeeds("test -f /root/alert-fired")
    second = show("braid-pcspkr-load.service", "ExecMainStartTimestampMonotonic")
    assert second != first, "second alert start must re-run the pcspkr loader"
    machine.succeed("systemctl stop braid-alert.service")
    wait_inactive("braid-beep.service")

with subtest("Beep unit hardening lands without hiding the speaker device"):
    unit = machine.succeed("systemctl cat braid-beep.service")
    assert show("braid-beep.service", "NoNewPrivileges") == "yes"
    assert show("braid-beep.service", "ProtectSystem") == "strict"
    assert show("braid-beep.service", "PrivateDevices") == "no"
    assert show("braid-beep.service", "PrivateNetwork") == "yes"
    assert show("braid-beep.service", "StartLimitIntervalUSec") == "0"
    assert "SystemCallFilter=" not in unit, (
        "beep unit must not render an explicit syscall filter:\n" + unit
    )
    assert beep_caps() == {"cap_setuid", "cap_setgid"}, (
        "beep unit must keep only setpriv drop capabilities, got: "
        + show("braid-beep.service", "CapabilityBoundingSet")
    )

with subtest("Alert services do not carry the beep sandbox"):
    for unit_name in ["braid-alert.service", "braid-alert-advisory.service"]:
        unit = machine.succeed("systemctl cat {}".format(unit_name))
        assert show(unit_name, "NoNewPrivileges") == "no"
        assert show(unit_name, "ProtectSystem") == "no"
        assert show(unit_name, "PrivateNetwork") == "no"
        for dropped in [
            "NoNewPrivileges=",
            "ProtectSystem=",
            "ProtectHome=",
            "PrivateTmp=",
            "RestrictNamespaces=",
            "SystemCallArchitectures=",
            "CapabilityBoundingSet=",
        ]:
            assert dropped not in unit, (
                "{} must not render {}:\n{}".format(unit_name, dropped, unit)
            )

with subtest("Privilege drop works under the beep unit capability set"):
    caps = " ".join(sorted(cap.upper() for cap in beep_caps()))
    setpriv = machine.succeed("command -v setpriv").strip()
    id_cmd = machine.succeed("command -v id").strip()
    out = machine.succeed(
        "systemd-run --quiet --pipe --wait --collect "
        "-p CapabilityBoundingSet='{}' "
        "-p NoNewPrivileges=yes "
        "{} --reuid=nobody --regid=beep --groups=beep -- {} -gn".format(
            caps, setpriv, id_cmd
        )
    ).strip()
    assert out == "beep", "setpriv must drop to beep group, got: " + out

with subtest("Lifecycle cascade starts and stops the beep"):
    machine.succeed("systemctl start braid-alert.service")
    machine.wait_until_succeeds("systemctl is-active braid-beep.service", timeout=30)
    machine.succeed("systemctl stop braid-alert.service")
    wait_inactive("braid-beep.service")

with subtest("Restart self-heals the beep loop but ack teardown still wins"):
    machine.succeed("systemctl start braid-alert.service")
    machine.wait_until_succeeds("systemctl is-active braid-beep.service", timeout=30)
    first_pid = show("braid-beep.service", "MainPID")
    assert first_pid not in ["", "0"], "beep unit must have a MainPID"
    machine.succeed(
        "systemctl kill --signal=KILL --kill-whom=main braid-beep.service"
    )
    machine.wait_until_succeeds(
        'pid="$(systemctl show braid-beep.service -p MainPID --value)"; '
        'test "$pid" != "0" -a "$pid" != "{}"'.format(first_pid),
        timeout=30,
    )
    machine.succeed("systemctl is-active braid-beep.service")
    machine.succeed("systemctl stop braid-alert.service")
    wait_inactive("braid-beep.service")

with subtest("alertCommand runs as root"):
    machine.succeed("rm -f /root/alert-fired")
    machine.succeed("systemctl start braid-alert.service")
    machine.wait_until_succeeds("test -f /root/alert-fired")
    machine.succeed("stat -c '%U' /root/alert-fired | grep root")
    machine.succeed("systemctl stop braid-alert.service")
    wait_inactive("braid-beep.service")

with subtest("Alert service can be started and stopped"):
    machine.succeed("systemctl start braid-alert.service")
    machine.succeed("systemctl is-active braid-alert.service")
    machine.wait_until_succeeds("systemctl is-active braid-beep.service", timeout=30)
    machine.succeed("systemctl stop braid-alert.service")
    wait_inactive("braid-alert.service")
    wait_inactive("braid-beep.service")

machine.shutdown()
