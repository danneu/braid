# Test: braid lock — handles btrfs scan registry on multi-device pool
#
# What: `braid lock` successfully closes all LUKS devices on a multi-device
# pool, even with the btrfs kernel scan registry holding references. Cycles
# lock/unlock 3 times to exercise the race window. Data survives.
#
# Why: After umount of a multi-device btrfs, the kernel's scan registry can
# hold transient references that block `cryptsetup close`. `braid lock` must
# call `btrfs device scan --forget` after umount to clear them reliably.
{ braid }:
{
  name = "braid-lock-btrfs-held";

  nodes.machine = { pkgs, ... }: {
    virtualisation.emptyDiskImages = [
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk2"; }
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk3"; }
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

  testScript = builtins.readFile ./braid-lock-btrfs-held.py;
}
