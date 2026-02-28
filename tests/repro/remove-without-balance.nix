# Repro: can btrfs device remove work on a 2-disk RAID1 without converting
# to single profile first?
#
# braid currently does: balance -dconvert=single → device remove
# This test checks whether skipping the balance and just running
# `btrfs device remove` works directly on a 2-device RAID1 pool.
{
  name = "repro-remove-without-balance";

  nodes.machine = { pkgs, ... }: {
    virtualisation.emptyDiskImages = [
      { size = 512; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 512; driveConfig.deviceExtraOpts.serial = "disk2"; }
    ];

    environment.systemPackages = [
      pkgs.cryptsetup
      pkgs.btrfs-progs
    ];
  };

  testScript = builtins.readFile ./remove-without-balance.py;
}
