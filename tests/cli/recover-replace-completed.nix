# Test: recover from interrupted replace (crash after replace completed)
#
# What: Verifies `braid recover` correctly rebuilds pool.json when a btrfs
# replace completed but the crash happened before pool.json was written.
# The live pool has the new disk; metadata still references the old one.
#
# Why: This is the most dangerous replace crash state. Recovery must discover
# the new disk in the live pool and resolve its by_id path from the journal's
# target_membership union — not the stale pool.json.
{ braid }:
{
  name = "recover-replace-completed";

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

  testScript = builtins.readFile ./recover-replace-completed.py;
}
