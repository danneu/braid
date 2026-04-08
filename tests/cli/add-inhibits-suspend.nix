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
# docs/decisions/inhibit-sleep.md for the boundary rule.
#
# 2 disks, each 1024 MiB. The test bootstraps a 1-disk pool, writes a
# 400 MiB single-profile payload, then adds the second disk so
# pool_balance_raid1 has real conversion work. Without the pre-add payload
# the balance has nothing to do and the inhibitor window collapses.
{ braid }:
{
  name = "add-inhibits-suspend";

  nodes.machine = { pkgs, ... }: {
    virtualisation.emptyDiskImages = [
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk2"; }
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
    + builtins.readFile ./add-inhibits-suspend.py;
}
