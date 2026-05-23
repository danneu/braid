# Test: replace-rejects-smaller-target
#
# What: `braid replace` refuses an undersized replacement before journal write
# or LUKS formatting, for both live and missing source devices.
#
# Why: btrfs rejects smaller replace targets only after braid used to format
# the target and write `pending-op.json`; braid now preflights the same size
# comparison before destructive work.
#
# Dependencies: braid add (builds the test pool), braid replace live and
# missing planning paths, real kernel `BTRFS_IOC_DEV_INFO`.
{ braid }:
{
  name = "replace-rejects-smaller-target";

  nodes.machine =
    { pkgs, ... }:
    {
      virtualisation.emptyDiskImages = [
        {
          size = 512;
          driveConfig.deviceExtraOpts.serial = "disk1";
        }
        {
          size = 512;
          driveConfig.deviceExtraOpts.serial = "disk2";
        }
        {
          size = 256;
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

  testScript = builtins.readFile ./replace-rejects-smaller-target.py;
}
