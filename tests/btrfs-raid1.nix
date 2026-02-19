# Test: btrfs-raid1
#
# What: LUKS-encrypts three drives, creates a btrfs RAID1 filesystem across
# them, mounts it, writes a file, reads it back, and verifies btrfs reports
# RAID1 data and metadata profiles.
#
# Why: This is the core storage primitive. If btrfs RAID1 on LUKS devices
# works, we have an encrypted self-healing pool. Everything else (samba,
# remote unlock, drive operations) builds on this.
#
# Dependencies: luks (LUKS format/open works).
{
  name = "btrfs-raid1";

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

  testScript = builtins.readFile ./btrfs-raid1.py;
}
