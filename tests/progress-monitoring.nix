# Test: progress-monitoring
#
# What: Starts btrfs balance and device-remove in background on larger disks,
# polls progress commands, and captures at least one in-progress sample from
# each operation. Does NOT wait for device-remove completion — only observes
# and captures mid-operation output.
#
# Why: Captures real in-progress btrfs output as golden fixtures for Rust
# parser tests. Completion semantics are tested elsewhere (btrfs-shrink,
# braid-remove-disk).
#
# Dependencies: LUKS + btrfs RAID1 creation and mount work (btrfs-raid1).
{
  name = "progress-monitoring";

  nodes.machine =
    { pkgs, ... }:
    {
      virtualisation.emptyDiskImages = [
        {
          size = 4096;
          driveConfig.deviceExtraOpts.serial = "disk1";
        }
        {
          size = 4096;
          driveConfig.deviceExtraOpts.serial = "disk2";
        }
        {
          size = 4096;
          driveConfig.deviceExtraOpts.serial = "disk3";
        }
      ];

      virtualisation.memorySize = 2048;

      environment.systemPackages = [
        pkgs.cryptsetup
        pkgs.btrfs-progs
        pkgs.lvm2
      ];
    };

  testScript = builtins.readFile ./progress-monitoring.py;
}
