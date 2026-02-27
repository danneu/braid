# Test: replace-dead-disk
#
# What: Dead disk replacement end-to-end covering both auto-detect (single
# missing device) and explicit --missing-id paths.
#
# Why: The original replace use case — swapping a failed drive — has zero VM
# coverage. Only unit tests cover the resolution logic. This exercises
# EvictionTarget::Missing and EvictionTarget::Devid end-to-end.
#
# Dependencies: braid add (builds the test pool), braid replace dead path.
{ braid }:
{
  name = "replace-dead-disk";

  nodes.machine = { pkgs, ... }: {
    virtualisation.emptyDiskImages = [
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk2"; }
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk3"; }
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk4"; }
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk5"; }
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
        disk4 = { by_id = "/dev/disk/by-id/virtio-disk4"; };
        disk5 = { by_id = "/dev/disk/by-id/virtio-disk5"; };
      };
      mount_point = "/mnt/storage";
    };
  };

  testScript = builtins.readFile ./replace-dead-disk.py;
}
