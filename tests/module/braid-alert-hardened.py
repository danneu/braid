# Test: braid-beep hardened service profile
#
# Intent: Verify the default audible alert loop runs under the hardened
#   braid-beep.service profile while retaining the capabilities needed for
#   setpriv to drop to the unprivileged beep identity.
#
# Why it exists: An empty or over-tight CapabilityBoundingSet would not stop
#   the loop from staying active, but it would make the beep call fail silently
#   because the script suppresses setpriv/beep stderr.
#
# Scenario: A NAS uses the default audible alert path with no custom
#   alertCommand. The latched orchestrator should start and stop the hardened
#   beep unit, and the privilege drop used by the wrapper should still succeed
#   under that sandbox.

start_all()
machine.wait_for_unit("multi-user.target")


def show(unit, prop):
    return machine.succeed(
        "systemctl show {} -p {} --value".format(unit, prop)
    ).strip()


def beep_caps():
    return set(show("braid-beep.service", "CapabilityBoundingSet").lower().split())


with subtest("beep service carries the hardened profile"):
    unit = machine.succeed("systemctl cat braid-beep.service")
    assert show("braid-beep.service", "ProtectSystem") == "strict"
    assert show("braid-beep.service", "ProtectHome") == "yes"
    assert show("braid-beep.service", "PrivateTmp") == "yes"
    assert show("braid-beep.service", "NoNewPrivileges") == "yes"
    assert show("braid-beep.service", "PrivateDevices") == "no"
    assert "CapabilityBoundingSet=CAP_SETUID" in unit, (
        "beep unit must render CAP_SETUID:\n" + unit
    )
    assert "CapabilityBoundingSet=CAP_SETGID" in unit, (
        "beep unit must render CAP_SETGID:\n" + unit
    )

with subtest("alert orchestrator starts and stops the hardened beep service"):
    machine.succeed("systemctl start braid-alert.service")
    machine.succeed("systemctl is-active braid-alert.service")
    machine.wait_until_succeeds("systemctl is-active braid-beep.service", timeout=30)
    machine.succeed("systemctl stop braid-alert.service")
    machine.wait_until_succeeds(
        "! systemctl is-active --quiet braid-beep.service", timeout=30
    )

with subtest("beep profile keeps only setuid and setgid capability"):
    caps = beep_caps()
    assert caps == {"cap_setuid", "cap_setgid"}, (
        "beep profile must keep only cap_setuid/cap_setgid, got: "
        + show("braid-beep.service", "CapabilityBoundingSet")
    )

with subtest("setpriv works under the beep unit capability set"):
    caps = " ".join(sorted(cap.upper() for cap in beep_caps()))
    setpriv = machine.succeed("command -v setpriv").strip()
    id_cmd = machine.succeed("command -v id").strip()
    out = machine.succeed(
        "systemd-run --quiet --pipe --wait --collect "
        "-p CapabilityBoundingSet='{}' "
        "-p NoNewPrivileges=yes "
        "{} --reuid=nobody --regid=beep --groups=beep -- {}".format(
            caps, setpriv, id_cmd
        )
    )
    assert "(nobody)" in out, "setpriv must drop to nobody, got: " + out
    assert "(beep)" in out, "setpriv must drop to beep group, got: " + out

machine.shutdown()
