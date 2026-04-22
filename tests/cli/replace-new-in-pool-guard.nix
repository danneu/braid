# Test: replace-new-in-pool-guard
#
# What: `braid replace` rejects --new with "already a member" when
# the new disk is already in the pool, at the braid layer (before
# reaching btrfs).
#
# Why: The live `btrfs replace start` path has no natural guard
# against replacing with an existing pool member. Without an explicit
# braid-level check, the command would pass the duplicate device to
# btrfs, risking corruption or a confusing btrfs-level error.
#
# Dependencies: braid add (builds the test pool), check_new_not_in_pool guard.
{ braid }:
{
  name = "replace-new-in-pool-guard";

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

  testScript = builtins.readFile ./replace-new-in-pool-guard.py;
}
