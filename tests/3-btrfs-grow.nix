# Test: btrfs-grow
#
# What: Creates a btrfs RAID1 on 2 LUKS devices, writes a file, then adds a
# 3rd drive to the live pool, rebalances, and verifies the pool grew and data
# is intact.
#
# Why: Proves the "buy drives incrementally" workflow. Start small, add drives
# later without reformatting or migrating data.
#
# Dependencies: btrfs-raid1 (LUKS + btrfs RAID1 creation and mount work).
{
  name = "btrfs-grow";

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

  testScript = builtins.readFile ./btrfs-grow.py;
}
