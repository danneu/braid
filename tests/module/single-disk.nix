# Test: braid-module-single-disk
#
# What: Enables the braid module with a single disk. An initrd fixture
# pre-formats the empty virtual drive as LUKS + single-disk btrfs. After boot,
# the test script unlocks the pool via `braid unlock --passphrase-stdin`.
#
# Why: Validates the module's core storage path — braid discover + unlock
# opening LUKS and mounting the pool — on the simplest possible pool
# (one disk, no RAID1).
#
# Dependencies: braid-module-disabled (module loads without error),
# hello-world (VM infra).
{ braid }:
{ pkgs, ... }:
let
  passphrase = "testpassphrase";
  diskNames = [ "disk1" ];
in
{
  name = "braid-module-single-disk";

  nodes.machine =
    { pkgs, ... }:
    {
      imports = [
        ../../modules/braid
        (import ./lib/initrd-fixture.nix {
          inherit passphrase diskNames;
          description = "Prepare LUKS + btrfs fixture (single disk)";
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
      ];
      virtualisation.memorySize = 2048;

      environment.systemPackages = [ pkgs.btrfs-progs ];

    };

  testScript = builtins.readFile ./single-disk.py;
}
