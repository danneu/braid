# Repro: soft RAID1 balance restores redundancy after degraded operation
#
# Proves that `btrfs balance start -dconvert=raid1,soft -mconvert=raid1,soft`
# converts single-profile chunks (created during degraded mode) back to RAID1
# without touching already-RAID1 chunks. This is the specific flag combination
# used by braid's `maybe_restore_raid1()` after `remove-missing` and `replace`
# (missing path).
#
# Extends the existing `degraded-writes-single` repro but tests the `,soft`
# variant instead of the non-soft `btrfs balance -dconvert=raid1 -mconvert=raid1`.
{
  name = "repro-degraded-soft-balance";

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

  testScript = builtins.readFile ./degraded-soft-balance.py;
}
