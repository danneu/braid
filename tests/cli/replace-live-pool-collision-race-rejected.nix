# Test: replace-live-pool-collision-race-rejected
#
# What: `braid replace` refuses a replacement target when a clone of that
# target's LUKS UUID is added to the mounted pool between confirmation and the
# pre-journal execution seam.
#
# Why: replace must re-check live-pool UUID ownership at execution time, not
# only during planning.
{ braid }:
{
  name = "replace-live-pool-collision-race-rejected";

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

  testScript = builtins.readFile ./replace-live-pool-collision-race-rejected.py;
}
