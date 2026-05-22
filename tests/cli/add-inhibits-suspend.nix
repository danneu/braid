# Test: add-inhibits-suspend
#
# What: braid add must hold a logind sleep inhibitor (What=sleep,
# Who=braid, Mode=block) for the duration of its mutation window —
# covering LUKS format/open of the new disk, btrfs device add, and the
# pool_balance_raid1 that converts pre-existing single-profile data to
# RAID1 when the post-add pool has ≥2 devices.
#
# Why: suspending mid-balance interrupts the conversion of single-profile
# chunks to RAID1, leaving new data unprotected. braid enables autosuspend
# by default, so this is reachable in normal operation. See
# docs/decisions/019-inhibit-sleep.md for the boundary rule.
#
# 2 disks, each 1024 MiB. The test bootstraps a 1-disk pool, writes a
# small single-profile payload, then adds the second disk through dm-delay
# so pool_balance_raid1 has observable conversion work without a large
# timing-only payload.
{ braid }:
{
  name = "add-inhibits-suspend";

  nodes.machine =
    { pkgs, ... }:
    {
      virtualisation.emptyDiskImages = [
        {
          size = 2048;
          driveConfig.deviceExtraOpts.serial = "disk1";
        }
        {
          size = 2048;
          driveConfig.deviceExtraOpts.serial = "disk2";
        }
      ];

      environment.systemPackages = [
        braid
        pkgs.cryptsetup
        pkgs.btrfs-progs
        pkgs.lvm2
      ];

      environment.etc."braid/config.json".text = builtins.toJSON {
        mount_point = "/mnt/storage";
      };
    };

  testScript =
    builtins.readFile ./../module/dm_delay_helpers.py + "\n\n"
    + builtins.readFile ./inhibitor_helpers.py + "\n\n"
    + builtins.readFile ./add-inhibits-suspend.py;
}
