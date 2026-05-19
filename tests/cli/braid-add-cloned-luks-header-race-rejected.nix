# Test: braid-add-cloned-luks-header-race-rejected
#
# What: `braid add` refuses a returned disk when a cloned-header mapper is
# added to the live pool between confirmation and the pool-add step.
#
# Why: returned-disk adoption must re-check live-pool LUKS UUID ownership at
# execution time, not only during planning.
{ braid }:
{
  name = "braid-add-cloned-luks-header-race-rejected";

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

  testScript = builtins.readFile ./braid-add-cloned-luks-header-race-rejected.py;
}
