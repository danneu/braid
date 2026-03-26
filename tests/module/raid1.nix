# Test: braid-module-raid1
#
# What: Enables the braid module with 3 disks. An initrd fixture pre-formats
# the empty virtual drives as LUKS + btrfs RAID1. After boot, the test script
# unlocks the pool via `braid unlock --passphrase-stdin`. Validates the mount,
# RAID1 profile, and runtime config.
#
# Why: Validates the module's storage path with the production-like 3-disk
# RAID1 configuration — braid discover + unlock opening all 3 LUKS devices
# and mounting the pool with correct RAID1 profile.
#
# Dependencies: braid-module-single-disk (single-disk path works),
# hello-world (VM infra).
{ braid }:
{ pkgs, ... }:
let
  passphrase = "testpassphrase";
  diskNames = [
    "disk1"
    "disk2"
    "disk3"
  ];
in
{
  name = "braid-module-raid1";

  nodes.machine =
    { pkgs, ... }:
    {
      imports = [
        ../../modules/braid
        (import ./lib/initrd-fixture.nix {
          inherit passphrase diskNames;
          description = "Prepare LUKS + btrfs RAID1 fixture";
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
        {
          size = 256;
          driveConfig.deviceExtraOpts.serial = "disk3";
        }
      ];
      virtualisation.memorySize = 2048;

      environment.systemPackages = [ pkgs.btrfs-progs ];

    };

  testScript = builtins.readFile ./raid1.py;
}
