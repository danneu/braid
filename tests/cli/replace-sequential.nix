# Test: replace-sequential
#
# What: Two sequential replacements in a 2-disk pool — the full migration
# workflow (replace disk1, then replace disk2).
#
# Why: The first replace may leave state (disk map, pool topology, LUKS
# mappers) that breaks the second. This is the real migration workflow
# users follow when upgrading all drives.
#
# Dependencies: braid add (builds the test pool), braid replace live path.
{ braid }:
{
  name = "replace-sequential";

  nodes.machine = { pkgs, ... }: {
    virtualisation.emptyDiskImages = [
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk2"; }
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk3"; }
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk4"; }
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
      };
      mount_point = "/mnt/storage";
    };
  };

  testScript = builtins.readFile ./replace-sequential.py;
}
