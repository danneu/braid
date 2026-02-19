# Test: btrfs-heal
#
# What: Creates a btrfs RAID1 on LUKS devices, writes a known file, corrupts
# the raw bytes on one underlying drive, then reads the file back and verifies
# btrfs auto-heals by returning correct data. Also checks btrfs device stats
# for detected corruption.
#
# Why: Auto-healing bit rot is the primary reason we chose btrfs RAID1 over
# mergerfs + SnapRAID. If this doesn't work, the entire architecture decision
# is wrong.
#
# Dependencies: btrfs-raid1 (LUKS + btrfs RAID1 creation and mount work).
{
  name = "btrfs-heal";

  nodes.machine = { pkgs, ... }: {
    virtualisation.emptyDiskImages = [
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk2"; }
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk3"; }
    ];

    virtualisation.memorySize = 2048;

    environment.systemPackages = [
      pkgs.cryptsetup
      pkgs.btrfs-progs
    ];
  };

  testScript = builtins.readFile ./btrfs-heal.py;
}
