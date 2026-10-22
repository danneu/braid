# Test: braid-lock-dead-member
#
# What: Runs `braid lock` on an unmounted pool after one member's backing disk
# was hot-unplugged, leaving its LUKS mapper open with `device: (null)`.
#
# Why: lock proved a mapper's owner by rereading the LUKS header through the
# backing disk, so a dead member's mapper was skipped and left open with its
# volume key in kernel memory. Lock must read the identity the mapper itself
# carries (its dm UUID) and close it as the member.
#
# Scenario: 2-disk RAID1 pool. disk1 is virtio; disk2 is a scsi_debug device,
# because virtio disks have no `device/delete` to simulate a hot-unplug. The
# pool is unmounted, disk2's SCSI device is deleted, and `braid lock` runs.
{ braid }:
{
  name = "braid-lock-dead-member";

  nodes.machine =
    { pkgs, ... }:
    {
      virtualisation.emptyDiskImages = [
        {
          size = 512;
          driveConfig.deviceExtraOpts.serial = "disk1";
        }
      ];

      environment.systemPackages = [
        braid
        pkgs.cryptsetup
        pkgs.btrfs-progs
      ];

      environment.etc."braid/config.json".text = builtins.toJSON {
        mount_point = "/mnt/storage";
      };
    };

  testScript = builtins.readFile ./braid-lock-dead-member.py;
}
