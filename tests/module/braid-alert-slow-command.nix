# Test: braid-alert slow alertCommand
#
# What: Validates that a hung alertCommand cannot delay or silence the
# braid-beep.service loop and cannot wedge the latched alert orchestrator.
#
# Why: Type=oneshot has no default start timeout. Without the timeout wrapper
# and parallel beep unit, a hung notifier could leave monitoring stuck in
# activating state or delay the audible alarm until the notifier exits.
#
# Scenario: NixOS machine with beep enabled and alertCommand set to sleep
# forever after writing a marker. Start the Critical alert path non-blocking,
# prove the beep loop starts while the command is still blocked, and prove the
# alert latch forms inside the configured timeout.
{ braid }:
{ pkgs, ... }:
let
  passphrase = "testpassphrase";
  diskNames = [
    "disk1"
    "disk2"
  ];
in
{
  name = "braid-alert-slow-command";

  nodes.machine =
    { pkgs, ... }:
    {
      imports = [
        ../../modules/braid
        (import ./lib/initrd-fixture.nix { inherit passphrase diskNames; })
      ];

      braid = {
        enable = true;
        package = braid;
        monitor.enable = true;
        monitor.alertCommand = "${pkgs.coreutils}/bin/touch /run/alert-command-started; ${pkgs.coreutils}/bin/sleep infinity";
        monitor.alertCommandTimeoutSec = 10;
      };

      virtualisation.emptyDiskImages = [
        {
          size = 512;
          driveConfig.deviceExtraOpts.serial = "disk1";
        }
        {
          size = 512;
          driveConfig.deviceExtraOpts.serial = "disk2";
        }
      ];
    };

  testScript = builtins.readFile ./braid-alert-slow-command.py;
}
