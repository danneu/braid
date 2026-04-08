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

with subtest("Service script has modprobe fallback and references the canonical beep wrapper"):
    # Verify the rendered service script includes the modprobe fallback
    # (for nixos-rebuild switch without reboot) and invokes the canonical
    # braid-beep-probe wrapper. The wrapper itself is the single source of
    # truth for the privilege-dropped beep argv: both this service script
    # and /etc/braid/notifier-config.json (consumed by `braid doctor`)
    # reference it by Nix store path so they cannot drift.
    #
    # The setpriv/reuid=nobody/regid=beep invariants moved into the wrapper
    # body when monitor.nix was refactored — we resolve the wrapper's store
    # path from the rendered alert script and assert against its contents
    # one indirection deeper.
    exec_start = machine.succeed(
        "systemctl cat braid-alert.service | grep '^ExecStart=' | sed 's/ExecStart=//'"
    ).strip()
    script = machine.succeed(f"cat {exec_start}")
    assert "modprobe" in script and "pcspkr" in script, "must include modprobe pcspkr fallback"
    assert "braid-beep-probe" in script, (
        f"alert script must reference the canonical braid-beep-probe wrapper:\n{script}"
    )

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
