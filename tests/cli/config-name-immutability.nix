# Test: config disk-name immutability
#
# What: Verifies mutating braid commands fail fast when a disk name is renamed
# in config while reusing the same by-id identity.
#
# Why: v1.0 treats disk names as immutable identity anchors. A name rename must
# not be silently reconciled by mutating commands.
#
# Dependencies: braid add (to build initial pool and disk-map entries).
{ braid }:
{
  name = "config-name-immutability";

  nodes.machine = { pkgs, ... }: {
    virtualisation.emptyDiskImages = [
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk2"; }
    ];

    environment.systemPackages = [
      braid
      pkgs.cryptsetup
      pkgs.btrfs-progs
      pkgs.jq
    ];

    environment.etc."braid/config.json".text = builtins.toJSON {
      disks = {
        disk1 = { by_id = "/dev/disk/by-id/virtio-disk1"; };
        disk2 = { by_id = "/dev/disk/by-id/virtio-disk2"; };
      };
      mount_point = "/mnt/storage";
    };
  };

  testScript = builtins.readFile ./config-name-immutability.py;
}

