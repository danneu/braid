# Repro: btrfs device remove missing — instant ENOSPC
#
# Reproduces btrfs failure mode 1: when surviving devices have zero
# unallocated space, `btrfs device remove missing` fails immediately
# with ENOSPC. The filesystem stays healthy.
#
# See docs/claude-enospc-vs-hang.md for full analysis.
{ braid }:
{
  name = "btrfs-remove-enospc";

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

  testScript = builtins.readFile ./btrfs-remove-enospc.py;
}
