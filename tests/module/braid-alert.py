# Test: braid-alert service lifecycle and PC speaker plumbing
#
# Intent: Verify that the braid monitor timer and alert service units
#   exist, can be started/stopped, and that the PC speaker setup
#   (pcspkr loader, privilege drop, alertCommand privileges) works.
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


def show(unit, prop):
    return machine.succeed(
        "systemctl show {} -p {} --value".format(unit, prop)
    ).strip()


with subtest("Monitor timer is active"):
    machine.succeed("systemctl is-active braid-monitor.timer")

with subtest("Alert service unit exists"):
    # The service should not be active by default (no alert yet),
    # but the unit file must be loadable.
    machine.succeed("systemctl cat braid-alert.service")

with subtest("Service script uses the canonical beep wrapper and no modprobe"):
    # Verify the rendered service script invokes the canonical
    # braid-beep-probe wrapper and no longer carries module-load privilege.
    # The wrapper itself is the single source of truth for the
    # privilege-dropped beep argv: both this service script and
    # /etc/braid/notifier-config.json (consumed by `braid doctor`) reference
    # it by Nix store path so they cannot drift. Runtime pcspkr loading now
    # lives in braid-pcspkr-load.service.
    #
    # The setpriv/reuid=nobody/regid=beep invariants moved into the wrapper
    # body when monitor.nix was refactored — we resolve the wrapper's store
    # path from the rendered alert script and assert against its contents
    # one indirection deeper.
    exec_start = machine.succeed(
        "systemctl cat braid-alert.service | grep '^ExecStart=' | sed 's/ExecStart=//'"
    ).strip()
    script = machine.succeed(f"cat {exec_start}")
    assert "modprobe" not in script, (
        "alert script must not load kernel modules:\n" + script
    )
    assert "braid-beep-probe" in script, (
        f"alert script must reference the canonical braid-beep-probe wrapper:\n{script}"
    )

    # Beep loop must implement exponential backoff capping at 900s (15min).
    # Catches a future refactor that silently reverts to fixed-cadence beeping.
    assert "delay=5" in script, f"alert script must initialize delay=5:\n{script}"
    assert "max_delay=900" in script, f"alert script must cap delay at 900s:\n{script}"
    assert "delay * 2" in script, f"alert script must double the delay each iter:\n{script}"
    assert "$max_delay" in script, f"alert script must clamp to max_delay:\n{script}"

    # The rendered script line looks like:
    #   /nix/store/xxxx-braid-beep-probe/bin/braid-beep-probe 2>/dev/null || true
    # Extract that absolute store path so we can read the wrapper body.
    wrapper_path = machine.succeed(
        f"grep -oE '/nix/store/[^[:space:]]*braid-beep-probe' {exec_start} | head -1"
    ).strip()
    assert wrapper_path.endswith("braid-beep-probe"), (
        f"could not extract wrapper path from alert script:\n{script}"
    )
    wrapper_body = machine.succeed(f"cat {wrapper_path}")
    assert "setpriv" in wrapper_body, f"wrapper must use setpriv for beep:\n{wrapper_body}"
    assert "reuid=nobody" in wrapper_body, f"wrapper must drop to nobody:\n{wrapper_body}"
    assert "regid=beep" in wrapper_body, f"wrapper must drop to beep group:\n{wrapper_body}"

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

    machine.succeed("rm -f /root/alert-fired")
    machine.succeed("systemctl start braid-alert.service")
    machine.wait_until_succeeds("test -f /root/alert-fired")
    second = show("braid-pcspkr-load.service", "ExecMainStartTimestampMonotonic")
    assert second != first, "second alert start must re-run the pcspkr loader"
    machine.succeed("systemctl stop braid-alert.service")

with subtest("custom alert command gets the light alert profile"):
    unit = machine.succeed("systemctl cat braid-alert.service")
    assert show("braid-alert.service", "NoNewPrivileges") == "yes"
    assert show("braid-alert.service", "ProtectKernelModules") == "yes"
    assert show("braid-alert.service", "ProtectControlGroups") == "yes"
    assert show("braid-alert.service", "ProtectKernelLogs") == "yes"
    assert show("braid-alert.service", "LockPersonality") == "yes"
    assert show("braid-alert.service", "RestrictSUIDSGID") == "yes"
    for dropped in [
        "ProtectSystem=",
        "ProtectHome=",
        "PrivateTmp=",
        "RestrictNamespaces=",
        "SystemCallArchitectures=",
        "CapabilityBoundingSet=",
    ]:
        assert dropped not in unit, (
            "custom alert command must not get {}:\n{}".format(dropped, unit)
        )
    assert show("braid-alert.service", "PrivateNetwork") == "no"

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
