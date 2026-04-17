# Test: fan-control-hotswap
#
# Intent: Verify that the udev-triggered reload chain works end-to-end: the
# braid-fan-reload oneshot, when started, restarts hddfancontrol-braid after
# the debounce sleep. Invoking the oneshot is exactly what the production
# udev rules do on SATA add/remove (via SYSTEMD_WANTS and RUN+=systemctl),
# so this exercises the same path the rules drive at runtime.
#
# Why it exists: The udev rule -> oneshot -> daemon restart chain is the
# most failure-prone part of fan control. A wiring-only test (grep rules
# files) verifies the rules' text but cannot catch breakage in the oneshot's
# ExecStartPre/ExecStart or in the daemon's restart semantics.
#
# The production udev rules (verified textually in the wiring test) cannot be
# exercised with a real SATA device inside a nixosTest VM -- the NixOS test
# framework's virtualisation.emptyDiskImages wires every drive as
# virtio-blk-pci, which surfaces as /dev/vdX with ID_BUS != "ata", so the
# ID_BUS=="ata" filter never matches. Bypassing that would require custom
# AHCI QEMU wiring that's hard to make reliable across architectures.
#
# Scenario: NixOS VM with braid.fanControl enabled and the daemon stubbed
# (sleep infinity) since there are no real hwmon/PWM devices in the VM.
# The test triggers braid-fan-reload via systemctl -- the same action the
# udev rules perform -- and verifies the daemon restarts (observed via
# ActiveEnterTimestamp change).
{ braid }:
{
  name = "fan-control-hotswap";

  nodes.machine = { pkgs, lib, ... }: {
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
      };
    };

    # Override the daemon with a test stub -- no real hwmon in the VM.
    systemd.services.hddfancontrol-braid.script = lib.mkForce ''
      exec ${pkgs.coreutils}/bin/sleep infinity
    '';
  };

  testScript = builtins.readFile ./fan-control-hotswap.py;
}
