# Test: btrfs-grow1
#
# What: Starts with a single-drive btrfs (no redundancy), adds a second drive
# and converts to RAID1, then adds a third. Verifies data survives each step.
#
# Why: This is the actual migration path. Start the NAS with one drive, copy
# data from Synology, then add drives as you buy them. Proves you don't need
# all drives on day one.
#
# Dependencies: btrfs-raid1 (btrfs RAID1 creation works).
{
  name = "btrfs-grow1";

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

  testScript = builtins.readFile ./btrfs-grow1.py;
}
