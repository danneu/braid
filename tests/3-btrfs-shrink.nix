# Test: btrfs-shrink
#
# What: Creates a 3-drive btrfs RAID1, writes data, removes a drive from the
# live pool, and verifies data is intact and the pool continues working with
# 2 drives.
#
# Why: Proves you can swap out a failing or undersized drive without downtime
# or data loss. btrfs migrates data off the drive before removing it.
#
# Dependencies: btrfs-raid1 (LUKS + btrfs RAID1 creation and mount work).
{
  name = "btrfs-shrink";

  nodes.machine = { pkgs, ... }: {
    virtualisation.emptyDiskImages = [
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk2"; }
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk3"; }
    ];

    environment.systemPackages = [
      pkgs.cryptsetup
      pkgs.btrfs-progs
    ];
  };

  testScript = builtins.readFile ./btrfs-shrink.py;
}
