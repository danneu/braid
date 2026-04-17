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
# Scenario: NixOS VM with braid.fanControl enabled and a fake PWM path. No real
# hwmon devices -- inspects generated unit files and udev rules, not runtime
# behavior.

start_all()
machine.wait_for_unit("multi-user.target")

with subtest("drivetemp kernel module is loaded"):
    machine.succeed("lsmod | grep -q drivetemp")

with subtest("hddfancontrol-braid service has correct arguments"):
    # NixOS generates a wrapper script for `script =` directives. Extract
    # the script path from ExecStart and read it.
    exec_start = machine.succeed(
        "systemctl show hddfancontrol-braid.service -p ExecStart --value"
    ).strip()
    script_path = exec_start.split("path=")[1].split(";")[0].strip()
    script = machine.succeed(f"cat {script_path}")
    assert "-d ata" in script, f"Expected '-d ata' in script:\n{script}"
    assert "/sys/class/hwmon/hwmon0/pwm1:65:60" in script, \
        f"Expected PWM path:minStart:maxStop in script:\n{script}"
    assert "--drive-temp-range 25 45" in script, f"Expected temp range in script:\n{script}"
    assert "--min-fan-speed-prct 10" in script, f"Expected min fan speed in script:\n{script}"
    assert "--interval 20s" in script, f"Expected interval in script:\n{script}"
    assert "--restore-fan-settings" in script, f"Expected --restore-fan-settings in script:\n{script}"

with subtest("hddfancontrol-braid has Restart=always and RestartSec=5s"):
    restart = machine.succeed(
        "systemctl show hddfancontrol-braid.service -p Restart --value"
    ).strip()
    assert restart == "always", f"Expected Restart=always, got {restart}"
    restart_sec = machine.succeed(
        "systemctl show hddfancontrol-braid.service -p RestartUSec --value"
    ).strip()
    assert restart_sec == "5s", f"Expected RestartUSec=5s, got {restart_sec}"

with subtest("no hddtemp daemon dependency"):
    after = machine.succeed(
        "systemctl show hddfancontrol-braid.service -p After --value"
    ).strip()
    assert "hddtemp" not in after, f"hddtemp found in After: {after}"
    machine.fail("systemctl cat hddtemp.service")

with subtest("braid-fan-reload oneshot exists with debounce"):
    unit = machine.succeed("systemctl cat braid-fan-reload.service")
    assert "restart hddfancontrol-braid.service" in unit
    assert "sleep 5" in unit

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

machine.shutdown()
