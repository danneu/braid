# Repro: degraded 3-disk RAID1 writes — single or RAID1 block groups?
#
# The 2-disk variant (degraded-writes-single) proved that losing 1 of 2 disks
# causes single-profile allocations. This test checks the 3-disk case: lose 1
# disk, mount degraded with 2 survivors. 2 disks is enough for RAID1 — does
# btrfs actually allocate RAID1 block groups, or does degraded mode still
# fall back to single?
{
  name = "repro-degraded-writes-3disk";

  nodes.machine =
    { pkgs, ... }:
    {
      virtualisation.emptyDiskImages = [
        {
          size = 512;
          driveConfig.deviceExtraOpts.serial = "disk1";
        }
        {
          size = 512;
          driveConfig.deviceExtraOpts.serial = "disk2";
        }
        {
          size = 512;
          driveConfig.deviceExtraOpts.serial = "disk3";
        }
      ];

      environment.systemPackages = [
        pkgs.cryptsetup
        pkgs.btrfs-progs
      ];
    };

  testScript = builtins.readFile ./degraded-writes-3disk.py;
}
