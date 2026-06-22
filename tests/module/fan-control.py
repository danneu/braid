# Test: fan-control (wiring)
#
# Intent: Verify that braid.fanControl generates the correct systemd service,
# hddfancontrol arguments, restart policy, and udev rules.
#
# Why it exists: The module defines a systemd service directly, loads drivetemp,
# generates udev hotswap rules, and creates a debounced restart oneshot. Any
# misconfiguration is silent until a user reports broken fan control on real
# hardware. This test catches wiring regressions at generation time.
#
# Scenario: NixOS VM with braid.fanControl enabled and a fake platform device
# and PWM number. No real hwmon devices -- inspects generated unit files and
# udev rules, not runtime behavior.

import json

start_all()
machine.wait_for_unit("multi-user.target")


def show(unit, prop):
    return machine.succeed(
        "systemctl show {} -p {} --value".format(unit, prop)
    ).strip()


with subtest("drivetemp kernel module is loaded"):
    machine.succeed("lsmod | grep -q drivetemp")

with subtest("hddfancontrol-braid service has correct arguments"):
    # NixOS generates a wrapper script for `script =` directives. Extract
    # the script path from ExecStart and read it.
    exec_start = show("hddfancontrol-braid.service", "ExecStart")
    script_path = exec_start.split("path=")[1].split(";")[0].strip()
    script = machine.succeed(f"cat {script_path}")
    assert "-d ata" in script, f"Expected '-d ata' in script:\n{script}"
    assert "--drive-temp-range 25 45" in script, f"Expected temp range in script:\n{script}"
    assert "--min-fan-speed-prct 10" in script, f"Expected min fan speed in script:\n{script}"
    assert "--interval 20s" in script, f"Expected interval in script:\n{script}"
    assert "--restore-fan-settings" in script, f"Expected --restore-fan-settings in script:\n{script}"

with subtest("resolver script references correct platform device and pwm number"):
    assert "/sys/devices/platform/braid-test.0/hwmon/hwmon*/device/pwm2" in script, \
        f"Expected hwmon/device/pwm fallback in script:\n{script}"
    assert "/sys/devices/platform/braid-test.0/hwmon/hwmon*/pwm2" in script, \
        f"Expected hwmon/pwm fallback in script:\n{script}"
    assert 'braid.fanControl: expected exactly one PWM path' in script, \
        f"Expected resolver failure message in script:\n{script}"
    assert ":65:60" in script, \
        f"Expected minStart:maxStop suffix on -p arg:\n{script}"

with subtest("hddfancontrol-braid has Restart=always and RestartSec=5s"):
    restart = show("hddfancontrol-braid.service", "Restart")
    assert restart == "always", f"Expected Restart=always, got {restart}"
    restart_sec = show("hddfancontrol-braid.service", "RestartUSec")
    assert restart_sec == "5s", f"Expected RestartUSec=5s, got {restart_sec}"

with subtest("hddfancontrol-braid carries shared hardening without PrivateDevices"):
    unit = machine.succeed("systemctl cat hddfancontrol-braid.service")
    assert "ProtectSystem=strict" in unit, (
        "hddfancontrol-braid must use ProtectSystem=strict:\n" + unit
    )
    assert show("hddfancontrol-braid.service", "NoNewPrivileges") == "yes"
    assert show("hddfancontrol-braid.service", "ProtectHome") == "yes"
    assert show("hddfancontrol-braid.service", "PrivateTmp") == "yes"
    assert show("hddfancontrol-braid.service", "MemoryDenyWriteExecute") == "yes"
    assert "SystemCallArchitectures=native" in unit, (
        "hddfancontrol-braid must keep native syscall arch:\n" + unit
    )
    assert "CPUSchedulingPolicy=rr" in unit, (
        "hddfancontrol-braid must keep rr scheduling:\n" + unit
    )
    assert show("hddfancontrol-braid.service", "PrivateDevices") == "no"
    assert "RestrictRealtime=" not in unit, (
        "base hardening must not block rr scheduling:\n" + unit
    )

with subtest("hddfancontrol-braid script starts inside the sandbox"):
    machine.wait_until_succeeds(
        "journalctl -b -u hddfancontrol-braid.service --no-pager "
        "| grep -q 'expected exactly one PWM path'"
    )

with subtest("no hddtemp daemon dependency"):
    after = show("hddfancontrol-braid.service", "After")
    assert "hddtemp" not in after, f"hddtemp found in After: {after}"
    machine.fail("systemctl cat hddtemp.service")

with subtest("braid-fan-reload oneshot exists with debounce"):
    unit = machine.succeed("systemctl cat braid-fan-reload.service")
    assert "restart hddfancontrol-braid.service" in unit
    assert "sleep 5" in unit
    assert "ProtectSystem=strict" in unit, (
        "braid-fan-reload must use ProtectSystem=strict:\n" + unit
    )
    assert "CapabilityBoundingSet=" in unit, (
        "braid-fan-reload must drop all capabilities:\n" + unit
    )
    assert "RestrictAddressFamilies=AF_UNIX" in unit, (
        "braid-fan-reload must restrict to AF_UNIX:\n" + unit
    )
    assert show("braid-fan-reload.service", "NoNewPrivileges") == "yes"

with subtest("udev rules have correct add and remove SATA hotswap rules"):
    rules = machine.succeed(
        "grep -r 'braid-fan-reload' /etc/udev/rules.d/"
    ).strip()
    assert 'ACTION=="add"' in rules
    assert 'ENV{SYSTEMD_WANTS}+="braid-fan-reload.service"' in rules
    assert 'ACTION=="remove"' in rules
    assert "systemctl start --no-block braid-fan-reload.service" in rules
    assert rules.count('ENV{ID_BUS}=="ata"') >= 2, \
        "Expected ID_BUS filter on both add and remove rules"

with subtest("braid CLI config.json includes fan_control with correct shape"):
    # The TUI reads /etc/braid/config.json at startup. Any drift between
    # the Nix option paths and the JSON keys would leave the fan section
    # silently disabled rather than producing a visible error.
    cfg = json.loads(machine.succeed("cat /etc/braid/config.json"))
    assert cfg["mount_point"] == "/mnt/storage"
    fc = cfg["fan_control"]
    assert fc["pwm"]["platform_device"] == "braid-test.0"
    assert fc["pwm"]["number"] == 2
    assert fc["pwm"]["min_start"] == 65
    assert fc["pwm"]["max_stop"] == 60
    assert fc["min_temp"] == 25
    assert fc["max_temp"] == 45
    assert fc["min_fan_speed_percent"] == 10
    # interval is daemon-only -- must not appear in CLI config
    assert "interval" not in fc, f"unexpected daemon-only key: {fc}"

machine.shutdown()
