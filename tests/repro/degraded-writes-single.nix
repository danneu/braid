# Repro: degraded RAID1 writes create single-profile block groups
#
# Reproduces a subtle btrfs pitfall: when a 2-disk RAID1 loses one disk and
# is mounted in degraded mode, new writes allocate single-profile block groups
# instead of RAID1 (only 1 disk left — can't maintain 2 copies). Those blocks
# have zero redundancy. After replacing the failed disk, you must run
# `btrfs balance start -dconvert=raid1 -mconvert=raid1` to restore full
# redundancy.
#
# Uses 2 disks (minimum RAID1) so losing one leaves only 1 surviving disk.
# A 3-disk RAID1 losing 1 still has 2 disks — enough for RAID1 — and
# wouldn't trigger this behavior.
{
  name = "repro-degraded-writes-single";

  nodes.machine = { pkgs, ... }: {
    virtualisation.emptyDiskImages = [
      { size = 512; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 512; driveConfig.deviceExtraOpts.serial = "disk2"; }
      { size = 512; driveConfig.deviceExtraOpts.serial = "disk3"; }
    ];

    environment.systemPackages = [
      pkgs.cryptsetup
      pkgs.btrfs-progs
    ];
  };

  testScript = builtins.readFile ./degraded-writes-single.py;
}
