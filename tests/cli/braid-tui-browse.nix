# Test: braid-tui-browse
#
# What: Boots a 2-disk RAID1 fixture pool, unlocks it, creates a subvolume,
# then drives the live `braid tui` Browse tab on tty2.
#
# Why: Replaces the old standalone parser canary with coverage for the real
# Browse-tab integration path and live btrfs subvolume parsing.
#
# Dependencies: braid-module-raid1 (RAID1 fixture setup).
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
  name = "braid-tui-browse";

  nodes.machine =
    { pkgs, ... }:
    {
      imports = [
        ../../modules/braid
        (import ../module/lib/initrd-fixture.nix {
          inherit passphrase diskNames;
          description = "Prepare LUKS + btrfs RAID1 fixture for tui Browse test";
        })
      ];

      braid = {
        enable = true;
        package = braid;
      };

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
      virtualisation.memorySize = 2048;

      environment.systemPackages = [ pkgs.btrfs-progs ];

      systemd.services.braid-tui-canary = {
        description = "Run braid tui on tty2 for the Browse VM canary";
        serviceConfig = {
          Type = "simple";
          ExecStart = "${braid}/bin/braid tui";
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

  testScript = builtins.readFile ./braid-tui-browse.py;
}
