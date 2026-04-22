# Test: braid monitor alert lifecycle after hot-unplug
#
# What: Validates that hot-unplugging a disk from a live RAID1 pool triggers
# the alert lifecycle correctly, even during the window before btrfs marks
# the device as MISSING.
#
# Why: On real hardware, hot-unplug caused braid monitor to exit 2 (error)
# instead of exit 1 (alert) because the LUKS mapper persisted with
# device: (null). No beep fired — the core alerting promise was broken.
#
# Scenario: 3-disk RAID1 pool. Two disks are virtio, the third is a
# scsi_debug device (removable SCSI disk). After the pool is mounted,
# the SCSI device is deleted via sysfs — this faithfully simulates SATA
# hot-unplug: the block device disappears, the LUKS dm mapper stays open
# with device: (null), and btrfs still reports the mapper path. braid
# monitor must detect this as a missing device, exit 1, and enable the
# full alert lifecycle including braid ack.
{ braid }:
{
  name = "monitor-hot-unplug";

  nodes.machine =
    { pkgs, ... }:
    {
      virtualisation.emptyDiskImages = [
        {
          size = 512;
          driveConfig.deviceExtraOpts.serial = "disk1";
        }
        {
          size = 512;
          driveConfig.deviceExtraOpts.serial = "disk3";
        }
      ];

      environment.systemPackages = [
        braid
        pkgs.cryptsetup
        pkgs.btrfs-progs
        pkgs.jq
      ];

      environment.etc."braid/config.json".text = builtins.toJSON {
        mount_point = "/mnt/storage";
      };
    };

  testScript = builtins.readFile ./monitor-hot-unplug.py;
}
