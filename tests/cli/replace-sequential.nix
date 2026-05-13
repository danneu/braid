# Test: replace-sequential
#
# What: Two sequential replacements in a 2-disk pool — the full migration
# workflow (replace disk1, then replace disk2).
#
# Why: The first replace may leave state (pool membership, pool topology, LUKS
# mappers) that breaks the second. This is the real migration workflow
# users follow when upgrading all drives.
#
# Dependencies: braid add (builds the test pool), braid replace live path.
{ braid }:
{
  name = "replace-sequential";

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

  testScript = builtins.readFile ./replace-sequential.py;
}
