# Test: btrfs-heal
#
# What: Creates a btrfs RAID1 on LUKS devices, writes a known file, then tests
# two healing paths:
#   1. Corrupt disk1, run btrfs scrub — verifies scheduled scrub repairs damage.
#   2. Corrupt disk2, read the file without scrub — verifies btrfs auto-heals
#      transparently on read via RAID1 checksums.
#
# Why: Auto-healing bit rot is the primary reason we chose btrfs RAID1 over
# mergerfs + SnapRAID. If this doesn't work, the entire architecture decision
# is wrong. A NAS user should never have to think about corruption — reads
# should silently return correct data, and periodic scrubs catch the rest.
#
# Dependencies: btrfs-raid1 (LUKS + btrfs RAID1 creation and mount work).
{
  name = "btrfs-heal";

  nodes.machine =
    { pkgs, ... }:
    {
      virtualisation.emptyDiskImages = [
        {
          size = 256;
          driveConfig.deviceExtraOpts.serial = "disk1";
        }
        {
          size = 256;
          driveConfig.deviceExtraOpts.serial = "disk2";
        }
        {
          size = 256;
          driveConfig.deviceExtraOpts.serial = "disk3";
        }
      ];

      virtualisation.memorySize = 2048;

      environment.systemPackages = [
        pkgs.cryptsetup
        pkgs.btrfs-progs
      ];
    };

  testScript = builtins.readFile ./btrfs-heal.py;
}
