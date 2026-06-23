# Test: recover from interrupted replace (crash before replace started)
#
# What: Verifies `braid recover` correctly rebuilds pool.json from live btrfs
# topology when a Replace journal exists but the replace never started.
#
# Why: The existing braid-recover test only covers Add journals. Replace
# journals create a union of pre + target memberships spanning both old and new
# devices. Recovery must correctly resolve by_id paths from the journal union.
{ braid }:
{
  name = "recover-replace-not-started";

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

  testScript =
    builtins.readFile ./member_helpers.py
    + "\n\n"
    + builtins.readFile ./recover-replace-not-started.py;
}
