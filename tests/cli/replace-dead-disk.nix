# Test: replace-dead-disk
#
# What: Dead disk replacement end-to-end covering both auto-detect (single
# missing device) and explicit --missing-id paths. Both use `btrfs replace
# start` to rebuild from RAID redundancy.
#
# Why: The original replace use case — swapping a failed drive — has zero VM
# coverage. Only unit tests cover the resolution logic. This exercises
# ReplaceSource::Missing end-to-end.
#
# Dependencies: braid add (builds the test pool), braid replace dead path.
{ braid }:
{
  name = "replace-dead-disk";

  nodes.machine =
    { pkgs, ... }:
    {
      virtualisation.emptyDiskImages = [
        {
          size = 1024;
          driveConfig.deviceExtraOpts.serial = "disk1";
        }
        {
          size = 1024;
          driveConfig.deviceExtraOpts.serial = "disk2";
        }
        {
          size = 1024;
          driveConfig.deviceExtraOpts.serial = "disk3";
        }
        {
          size = 1024;
          driveConfig.deviceExtraOpts.serial = "disk4";
        }
        {
          size = 1024;
          driveConfig.deviceExtraOpts.serial = "disk5";
        }
        {
          size = 1024;
          driveConfig.deviceExtraOpts.serial = "disk6";
        }
      ];

      environment.systemPackages = [
        braid
        pkgs.cryptsetup
        pkgs.btrfs-progs
      ];

      environment.etc."braid/config.json".text = builtins.toJSON {
        mount_point = "/mnt/storage";
      };
    };

  testScript = builtins.readFile ./replace-dead-disk.py;
}
