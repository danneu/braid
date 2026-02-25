# Test: braid add lifecycle
#
# What: Runs `braid add <name>` through its full lifecycle: first disk (creates
# pool), second disk (converts to RAID1), third disk (expands pool), plus
# validation errors, pre-formatted disk handling, and idempotent re-add.
#
# Why: `braid add` is the primary path for LUKS format + pool join. Every
# primitive has been proven in isolation (luks, btrfs-raid1, grow, shrink, heal,
# degrade). This test proves the intent CLI ties them together correctly.
#
# Dependencies: btrfs-grow1 (single -> RAID1 -> 3-drive works manually).
{ braid }:
{
  name = "braid-add-disk";

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

  testScript = builtins.readFile ./braid-add-disk.py;
}
