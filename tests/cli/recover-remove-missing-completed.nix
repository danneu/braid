# Test: recover from interrupted remove-missing after btrfs commit
#
# What: Verifies `braid recover` completes a RemoveMissing::PoolMutation
# journal once btrfs has already removed the missing devid.
#
# Why: This pins the VM path for degraded mount probing, real btrfs missing
# device removal, by-id membership resolution, and pending-op cleanup.
{ braid }:
{
  name = "recover-remove-missing-completed";

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

  testScript = builtins.readFile ./recover-remove-missing-completed.py;
}
