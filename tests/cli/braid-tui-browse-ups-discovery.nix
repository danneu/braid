# Test: braid-tui-browse-ups-discovery
#
# What: Boots a single-disk fixture with braid.ups.enable = false, creates
# pool membership, then drives the live Browse tab to NUT > UPSes.
#
# Why: NUT discovery must work before the UPS module is enabled; otherwise
# users cannot discover the name they need to put in braid.ups.name from the TUI.
#
# Dependencies: initrd-fixture (LUKS-formats the disk before boot).
{ braid }:
{ pkgs, ... }:
let
  passphrase = "testpassphrase";
  diskNames = [ "disk1" ];
in
{
  name = "braid-tui-browse-ups-discovery";

  nodes.machine =
    { pkgs, ... }:
    {
      imports = [
        ../../modules/braid
        (import ../module/lib/initrd-fixture.nix {
          inherit passphrase diskNames;
          description = "Prepare LUKS + btrfs fixture for UPS discovery Browse test";
        })
      ];

      braid = {
        enable = true;
        package = braid;
        ups.enable = false;
      };

      virtualisation.emptyDiskImages = [
        {
          size = 256;
          driveConfig.deviceExtraOpts.serial = "disk1";
        }
      ];
      virtualisation.memorySize = 2048;

      systemd.services.braid-tui-canary = {
        description = "Run braid tui on tty2 for the Browse UPS discovery canary";
        serviceConfig = {
          Type = "simple";
          ExecStart = "/run/current-system/sw/bin/braid tui";
          Environment = "TERM=linux";
          StandardInput = "tty-force";
          StandardOutput = "tty";
          StandardError = "tty";
          TTYPath = "/dev/tty2";
          TTYReset = true;
          TTYVHangup = true;
          TTYVTDisallocate = true;
        };
      };
    };

  testScript = builtins.readFile ./braid-tui-browse-ups-discovery.py;
}
