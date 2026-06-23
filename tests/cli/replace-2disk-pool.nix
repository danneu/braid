# Test: replace-2disk-pool
#
# What: Replace a disk in a 2-disk RAID1 pool (the most common setup).
#
# Why: The existing replace-live-disk test uses a 3-drive pool. A 2-disk pool
# goes through a different temporary topology (2 → 3 → 2) and is the most
# common real-world configuration.
#
# Dependencies: braid add (builds the test pool), braid replace live path.
{ braid }:
{
  name = "replace-2disk-pool";

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

  testScript =
    builtins.readFile ./member_helpers.py
    + "\n\n"
    + builtins.readFile ./replace-2disk-pool.py;
}
