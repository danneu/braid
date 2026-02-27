# Test: replace-new-already-in-pool
#
# What: `--new` disk that's already a pool member is rejected.
#
# Why: No explicit braid-level check exists; failure comes from the btrfs
# layer. This documents current behavior and may reveal need for a
# pre-check in replace.rs.
#
# Dependencies: braid add (builds the test pool), braid replace validation.
{ braid }:
{
  name = "replace-new-already-in-pool";

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

  testScript = builtins.readFile ./replace-new-already-in-pool.py;
}
