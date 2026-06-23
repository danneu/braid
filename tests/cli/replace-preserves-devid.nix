# Test: replace-preserves-devid
#
# What: After live-disk replacement, the new device inherits the old device's
# devid (proving `btrfs replace` was used, not add+balance+remove). Also
# verifies resize to full capacity when the new disk is larger.
#
# Why: The add+balance+remove approach assigns a new devid (e.g., 3) to the
# added device. `btrfs replace` preserves the devid (stays 2). This behavioral
# difference proves the code is using the fast path.
#
# Dependencies: braid add (builds the test pool), braid replace live path.
{ braid }:
{
  name = "replace-preserves-devid";

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
    builtins.readFile ./member_helpers.py + "\n\n" + builtins.readFile ./replace-preserves-devid.py;
}
