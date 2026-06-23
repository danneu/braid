# Test: replace-larger-disk
#
# What: Replace a disk with a larger one and verify btrfs uses the full
# capacity of the new drive (not capped at the old drive's size).
#
# Why: The real-world migration scenario (e.g. 2x12TB → 2x20TB). No test
# verifies that btrfs exposes the full size after a replace-based upgrade.
#
# Dependencies: braid add (builds the test pool), braid replace live path.
{ braid }:
{
  name = "replace-larger-disk";

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
    + builtins.readFile ./replace-larger-disk.py;
}
