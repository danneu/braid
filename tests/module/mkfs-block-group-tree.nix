# Test: mkfs-block-group-tree
#
# Intent: Bootstraps one single-disk pool and one RAID1 pool through braid add,
# then checks the created btrfs superblocks for BLOCK_GROUP_TREE.
#
# Why it exists: braid requests the `block-group-tree` bit explicitly at mkfs
# time so the on-disk feature set never depends on the linked btrfs-progs
# default. The bit is the btrfs-progs 6.19 default that braid's pinned
# nixos-26.05 toolchain ships; this fails closed if it is ever absent. The rest
# of the feature set still tracks btrfs-progs defaults; ADR-027.
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
