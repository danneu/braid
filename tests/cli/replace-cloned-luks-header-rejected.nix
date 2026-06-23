# Test: replace-cloned-luks-header-rejected
#
# What: `braid replace` refuses an already-open new-target mapper whose
# backing path is not the configured by-id disk, even when the backing disk
# has a cloned LUKS UUID.
#
# Why: cloned LUKS headers duplicate UUID identity; the live mapper boundary
# must also verify the backing block-device path before btrfs replace starts.
{ braid }:
{
  name = "replace-cloned-luks-header-rejected";

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
    + builtins.readFile ./replace-cloned-luks-header-rejected.py;
}
