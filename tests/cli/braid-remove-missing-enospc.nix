# Test: braid remove-missing ENOSPC pre-flight
#
# What: Verifies that `braid remove-missing` rejects the operation when
# surviving devices lack unallocated space to absorb relocation.
#
# Why: Without this check, `braid remove-missing` delegates to
# `btrfs device remove missing` which hangs for hours, then crashes
# the filesystem to read-only (transaction abort). This test ensures
# braid detects the condition and fails fast with actionable guidance.
#
# Scenario: Models a real incident — 3-drive RAID1 pool ~80% full, one drive
# dies. Surviving drives have all data mirrored but not enough unallocated
# space for btrfs to relocate block groups off the dead device.
{ braid }:
{
  name = "braid-remove-missing-enospc";

  nodes.machine = { pkgs, ... }: {
    virtualisation.emptyDiskImages = [
      { size = 512; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 512; driveConfig.deviceExtraOpts.serial = "disk2"; }
      { size = 512; driveConfig.deviceExtraOpts.serial = "disk3"; }
    ];

    environment.systemPackages = [
      braid
      pkgs.cryptsetup
      pkgs.btrfs-progs
    ];

    environment.etc."braid/config.json".text = builtins.toJSON {
      disks = {
        disk1 = { by_id = "/dev/disk/by-id/virtio-disk1"; };
        disk2 = { by_id = "/dev/disk/by-id/virtio-disk2"; };
        disk3 = { by_id = "/dev/disk/by-id/virtio-disk3"; };
      };
      mount_point = "/mnt/storage";
    };
  };

  testScript = builtins.readFile ./braid-remove-missing-enospc.py;
}
