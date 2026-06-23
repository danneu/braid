# Test: returned disk add after remove-missing
#
# What: Builds a 3-disk braid pool, removes disk3 as missing, then re-adds the
# same physical disk through `braid add`.
#
# Why: A returned braid-labeled disk still carries a stale same-FSID btrfs
# signature after `remove-missing`. The add command must verify that identity
# and use the returned-disk force path, not require a manual wipe.
#
# Dependencies: braid add and remove-missing.
{ braid }:
{
  name = "add-returned-disk-after-remove-missing";

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

  testScript =
    builtins.readFile ./member_helpers.py
    + "\n\n"
    + builtins.readFile ./add-returned-disk-after-remove-missing.py;
}
