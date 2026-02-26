# Test: braid remove-missing ENOSPC hang (slow relocation failure)
#
# What: Verifies that `braid remove-missing` rejects the operation when
# surviving devices have SOME unallocated space but not enough to complete
# relocation — the scenario where `btrfs device remove missing` hangs
# instead of failing fast.
#
# Why: This is the scariest variant of the ENOSPC problem. Unlike the
# "instantly full" case (braid-remove-missing-enospc), here btrfs starts
# relocating block groups, partially succeeds, then gets stuck retrying
# the remaining ones in a loop. On real hardware with slow USB drives,
# this hangs for hours before crashing the filesystem to read-only.
# In a VM with fast I/O, btrfs may cycle through retries quickly and
# fail rather than hang indefinitely — but the pre-flight check must
# catch the condition either way.
#
# Scenario: Models the real incident: 3-drive RAID1 pool, moderately
# full, one drive dies. Surviving drives have 300-600MiB free each —
# enough for btrfs to begin block group relocation but not to finish.
{ braid }:
{
  name = "braid-remove-missing-enospc-hang";

  nodes.machine = { pkgs, ... }: {
    virtualisation.emptyDiskImages = [
      { size = 4096; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 4096; driveConfig.deviceExtraOpts.serial = "disk2"; }
      { size = 4096; driveConfig.deviceExtraOpts.serial = "disk3"; }
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

  testScript = builtins.readFile ./braid-remove-missing-enospc-hang.py;
}
