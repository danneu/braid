# Test: multi-disk braid add
#
# What: Tests `braid add disk1 disk2` (multi-disk add) through three scenarios:
# (1) bootstrap a new pool with 2 disks → RAID1 from the start, no balance;
# (2) add 2 more disks to existing pool → one balance at the end;
# (3) single-disk add → backward compat.
#
# Why: Multi-disk add is the recommended way to start a pool. It avoids the
# single→RAID1 balance that rewrites all data. This test proves the mkfs.btrfs
# -d raid1 -m raid1 path works end-to-end.
#
# Dependencies: braid-add-disk (single-disk add works).
{ braid }:
{
  name = "multi-add";

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
        {
          size = 1024;
          driveConfig.deviceExtraOpts.serial = "disk5";
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

  testScript = builtins.readFile ./multi-add.py;
}
