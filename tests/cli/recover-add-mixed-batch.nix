# Test: recover from interrupted mixed-batch add
#
# What: Verifies `braid recover` completes an interrupted add where one target
# is already committed to btrfs and another target still needs replay.
#
# Why: Recovery must use the live pool to skip committed add targets, replay
# missing targets, rebuild pool.json, and clear stale acked-stats entries for
# every journaled add target.
{ braid }:
{
  name = "recover-add-mixed-batch";

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

  testScript = builtins.readFile ./recover-add-mixed-batch.py;
}
