# Test: mkfs-block-group-tree
#
# Intent: Bootstraps one single-disk pool and one RAID1 pool through braid add,
# then checks the created btrfs superblocks for BLOCK_GROUP_TREE.
#
# Why it exists: braid pins the `block-group-tree` bit specifically so pools
# created with nixos-25.11's btrfs-progs 6.17.1 carry the same bit that the
# nixos-26.05-era btrfs-progs 6.19.1 default set enables. The rest of the
# feature set still tracks btrfs-progs defaults; ADR-027.
#
# Scenario: First-time add creates fresh encrypted btrfs pools on one-disk and
# two-disk layouts, then the underlying mapper devices expose the expected
# compat_ro feature flag.
{ braid }:
{ pkgs, ... }:
let
  commonNode =
    { pkgs, ... }:
    {
      imports = [ ../../modules/braid ];

      braid = {
        enable = true;
        package = braid;
      };

      virtualisation.memorySize = 2048;

      environment.systemPackages = [
        pkgs.btrfs-progs
      ];
    };
in
{
  name = "mkfs-block-group-tree";

  nodes.single =
    { ... }:
    {
      imports = [ commonNode ];

      virtualisation.emptyDiskImages = [
        {
          size = 256;
          driveConfig.deviceExtraOpts.serial = "disk1";
        }
      ];
    };

  nodes.raid1 =
    { ... }:
    {
      imports = [ commonNode ];

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

  testScript = builtins.readFile ./mkfs-block-group-tree.py;
}
