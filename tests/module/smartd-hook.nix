# Test: smartd-hook
#
# What: Verifies the smartd exec hook script that bridges smartd into braid's
# alert system — its rendered contents, invocation behavior, and ack cleanup.
#
# Why: The smartd-config test validates config composition (no duplicate
# directives), and braid-smartd-alert tests the CLI flag-file model, but
# nothing invokes the actual hook script that smartd would call or verifies
# it creates the flag file and starts braid-alert.service.
{ braid }:
{ pkgs, lib, ... }:
{
  name = "smartd-hook";

  nodes.machine =
    { pkgs, ... }:
    {
      imports = [ ../../modules/braid ];

      braid = {
        enable = true;
        package = braid;
        monitor.enable = true;
        monitor.beep = false;
        monitor.alertCommand = "touch /root/alert-fired";
      };

      # Minimal disks — smartd config generation needs the module to evaluate,
      # but the test does not exercise storage.
      virtualisation.emptyDiskImages = [
        {
          size = 256;
          driveConfig.deviceExtraOpts.serial = "disk1";
        }
        {
          size = 256;
          driveConfig.deviceExtraOpts.serial = "disk2";
        }
      ];
    };

  testScript = builtins.readFile ./smartd-hook.py;
}
