# Test: btrfs-degrade
#
# What: Creates a 3-drive btrfs RAID1, writes data, simulates a drive failure
# by closing the LUKS device, then remounts in degraded mode and verifies
# data is still readable.
#
# Why: Proves the pool survives a sudden drive loss. This is the "drive dies
# at 3am" scenario — the NAS should keep serving files until you replace it.
#
# Dependencies: btrfs-raid1 (LUKS + btrfs RAID1 creation and mount work).
{
  name = "btrfs-degrade";

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

      environment.systemPackages = [
        pkgs.cryptsetup
        pkgs.btrfs-progs
      ];
    };

  testScript = builtins.readFile ./btrfs-degrade.py;
}
