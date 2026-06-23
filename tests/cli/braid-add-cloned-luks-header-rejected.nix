# Test: braid-add-cloned-luks-header-rejected
#
# What: `braid add` refuses an already-open target mapper whose backing path
# is not the configured by-id disk, even when the backing disk has a cloned
# LUKS UUID and label.
#
# Why: returned-disk adoption must not trust UUID alone at the live mapper
# boundary; cloned headers intentionally duplicate UUID identity.
{ braid }:
{
  name = "braid-add-cloned-luks-header-rejected";

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
    + builtins.readFile ./braid-add-cloned-luks-header-rejected.py;
}
