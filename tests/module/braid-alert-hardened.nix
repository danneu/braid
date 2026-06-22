# Test: braid-alert hardened service profile
#
# What: Validates the strong braid-alert.service profile used when no custom
# alertCommand is configured.
#
# Why: The default beep-only alert path should get the shared systemd hardening
# base, but it must not drop the setuid/setgid capabilities before the beep
# wrapper can drop privileges with setpriv.
#
# Scenario: NixOS machine with braid.monitor enabled, beep enabled, and no
# custom alertCommand. Verify the alert service starts and its sandbox still
# permits the privilege drop used by the beep wrapper.
{ braid }:
{ ... }:
let
  passphrase = "testpassphrase";
  diskNames = [
    "disk1"
    "disk2"
  ];
in
{
  name = "braid-alert-hardened";

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

  testScript = builtins.readFile ./braid-alert-hardened.py;
}
