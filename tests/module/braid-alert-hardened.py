# Test: braid-alert hardened service profile
#
# Intent: Verify the default beep-only braid-alert.service runs under the
# shared hardening base while retaining the capabilities needed for setpriv to
# drop to the unprivileged beep identity.
#
# Why it exists: An empty or over-tight CapabilityBoundingSet would not stop the
# service loop from staying active, but it would make the beep call fail
# silently because the script suppresses setpriv/beep stderr.
#
# Scenario: A NAS uses the default audible alert path with no custom
# alertCommand. The unit should be strongly sandboxed, and the exact privilege
# drop used by the beep wrapper should still succeed under that sandbox.

start_all()
machine.wait_for_unit("multi-user.target")


def show(unit, prop):
    return machine.succeed(
        "systemctl show {} -p {} --value".format(unit, prop)
    ).strip()


with subtest("strong alert profile carries the shared base"):
    unit = machine.succeed("systemctl cat braid-alert.service")
    assert show("braid-alert.service", "ProtectSystem") == "strict"
    assert show("braid-alert.service", "ProtectHome") == "yes"
    assert show("braid-alert.service", "PrivateTmp") == "yes"
    assert show("braid-alert.service", "NoNewPrivileges") == "yes"
    assert "CapabilityBoundingSet=" not in unit, (
        "strong alert profile must not empty the setpriv capabilities:\n" + unit
    )

with subtest("strong alert service starts under the sandbox"):
    machine.succeed("systemctl start braid-alert.service")
    machine.succeed("systemctl is-active braid-alert.service")
    machine.succeed("systemctl stop braid-alert.service")

with subtest("strong alert profile keeps setuid and setgid capability"):
    caps = show("braid-alert.service", "CapabilityBoundingSet").lower()
    assert "cap_setuid" in caps, (
        "strong alert profile must keep cap_setuid for setpriv, got: " + caps
    )
    assert "cap_setgid" in caps, (
        "strong alert profile must keep cap_setgid for setpriv, got: " + caps
    )

with subtest("setpriv works under the strong alert sandbox"):
    setpriv = machine.succeed("command -v setpriv").strip()
    id_cmd = machine.succeed("command -v id").strip()
    out = machine.succeed(
        "systemd-run --quiet --pipe --wait --collect "
        "-p NoNewPrivileges=yes "
        "-p ProtectSystem=strict "
        "-p ProtectHome=yes "
        "-p PrivateTmp=yes "
        "-p ProtectControlGroups=yes "
        "-p ProtectKernelModules=yes "
        "-p ProtectKernelLogs=yes "
        "-p RestrictNamespaces=yes "
        "-p LockPersonality=yes "
        "-p MemoryDenyWriteExecute=yes "
        "-p SystemCallArchitectures=native "
        "-p RestrictSUIDSGID=yes "
        "{} --reuid=nobody --regid=beep --groups=beep -- {}".format(
            setpriv, id_cmd
        )
    )
    assert "(nobody)" in out, "setpriv must drop to nobody, got: " + out
    assert "(beep)" in out, "setpriv must drop to beep group, got: " + out

machine.shutdown()
