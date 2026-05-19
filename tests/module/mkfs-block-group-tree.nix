# Test: mkfs-block-group-tree
#
# What: Bootstraps one single-disk pool and one RAID1 pool through braid add,
# then checks the created btrfs superblocks for BLOCK_GROUP_TREE.
#
# Why: braid pins mkfs.btrfs feature flags explicitly so new pool feature bits
# do not depend on nixpkgs' btrfs-progs default set.
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
