# Test: remove-missing-inhibits-suspend
#
# What: braid remove-missing must hold a logind sleep inhibitor
# (What=sleep, Who=braid, Mode=block) for the duration of its mutation
# window — covering both the fast `btrfs device remove <devid>` and the
# long-running soft RAID1 balance that maybe_restore_raid1 triggers when
# clearing the last missing device on a multi-disk pool.
#
# Why: suspending mid-soft-balance interrupts the conversion of
# single-profile chunks (created during degraded operation) back to RAID1
# and can leave new data unprotected. braid enables autosuspend by default,
# so this is reachable in normal operation. See
# docs/design/decisions/019-inhibit-sleep.md for the boundary rule.
#
# 3 disks, each 512 MiB. The 3-disk pool is the minimum that satisfies
# maybe_restore_raid1's "≥2 surviving devices" gate after a 1-missing
# remove-missing.
#
# Subtle point: in a 3-disk RAID1 pool with only 1 missing device, btrfs
# can still write RAID1 chunks (2 surviving disks are enough for the
# minimum mirror count), so degraded writes do NOT produce
# single-profile chunks the way the proven degraded-soft-balance.py 2-disk
# scenario does. The soft balance fired by maybe_restore_raid1 is
# therefore a near-no-op in this test — the goal is to verify the
# inhibitor *wiring* through cmd_remove_missing's mutation window, not
# to make the soft-balance phase long. The .py test uses tight polling
# to catch the inhibitor's brief window during the fast operation.
#
# The missing-disk simulation reuses the canonical pattern from
# tests/cli/braid-remove-disk.py (umount → cryptsetup close → degraded
# mount).
{ braid }:
{
  name = "remove-missing-inhibits-suspend";

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
        braid
        pkgs.cryptsetup
        pkgs.btrfs-progs
      ];

      environment.etc."braid/config.json".text = builtins.toJSON {
        mount_point = "/mnt/storage";
      };
    };

  testScript =
    builtins.readFile ./inhibitor_helpers.py
    + "\n\n"
    + builtins.readFile ./remove-missing-inhibits-suspend.py;
}
