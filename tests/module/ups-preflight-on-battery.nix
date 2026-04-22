# Test: ups-preflight-on-battery
#
# What: Boots the VM with braid.ups.enable = true and a dummy-ups UPS that
# reports "OB LB" (on battery, low battery). Asserts that `braid add`
# refuses at preflight with a Validation-shaped error.
#
# Why: one of the two v1 safety guarantees is that pool-mutating commands
# refuse to start on battery. Covered in unit tests against MockRunner;
# this VM test proves the same refusal against real `upsc` output from
# a real NUT deployment (real upsd + upsmon + dummy-ups driver wired via
# the braid module). A unit-test-only proof is not enough because
# `config.ups` plumbing, CmdRequest::UpscQuery dispatch, and the
# module's `power.ups.*` values all have to agree for the check to fire
# end-to-end.
{ braid }:
{ pkgs, lib, ... }:
{
  name = "ups-preflight-on-battery";

  nodes.machine =
    { pkgs, ... }:
    {
      imports = [ ../../modules/braid ];

      braid = {
        enable = true;
        package = braid;
        ups = {
          enable = true;
          name = "ups";
          driver = "dummy-ups";
          port = "ups.dev";
        };
      };

      # dummy-ups reads a .dev file from NUT_CONFPATH (/etc/nut). The
      # `.dev` extension selects dummy-once mode. Status is OB (on
      # battery) but NOT LB (low battery), so upsmon does not trigger
      # its critical-state SHUTDOWNCMD before the test can run. braid
      # preflight refuses on OB *or* LB (see check_ups_not_on_battery),
      # so OB alone is sufficient to exercise the refusal path.
      environment.etc."nut/ups.dev".text = ''
        device.mfr: Dummy
        device.model: OB-fixture
        ups.status: OB
        battery.charge: 80
        battery.charge.low: 10
        ups.load: 20
      '';

      virtualisation.emptyDiskImages = [
        {
          size = 256;
          driveConfig.deviceExtraOpts.serial = "disk1";
        }
      ];
      virtualisation.memorySize = 2048;

      environment.systemPackages = [
        pkgs.btrfs-progs
        pkgs.nut
      ];
    };

  testScript = builtins.readFile ./ups-preflight-on-battery.py;
}
