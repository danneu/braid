# Test: fan-control
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
{ braid }:
{
  name = "fan-control";

  nodes.machine = { ... }: {
    imports = [ ../../modules/braid ];

    braid = {
      enable = true;
      package = braid;

      fanControl = {
        enable = true;
        pwm = {
          platformDevice = "braid-test.0";
          number = 2;
        };
        minStart = 65;
        maxStop = 60;
        minTemp = 25;
        maxTemp = 45;
        minFanSpeedPercent = 10;
        interval = "20s";
      };
    };
  };

  testScript = builtins.readFile ./fan-control.py;
}
