# Test: braid-remove-disk
#
# What: Runs braid-remove-disk through its lifecycle: graceful remove, remove-missing,
# LUKS cleanup, redundancy warning, and validation errors.
#
# Why: Symmetric counterpart to braid-add-disk. Must handle both happy path (disk
# present, data migrates off) and failure path (disk gone, remove missing).
#
# Dependencies: braid-add-disk (builds the test pool).
{
  name = "braid-remove-disk";

  nodes.machine = { pkgs, ... }: {
    virtualisation.emptyDiskImages = [
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk2"; }
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk3"; }
    ];

    environment.systemPackages = [
      (pkgs.writeShellApplication {
        name = "braid-add-disk";
        runtimeInputs = [ pkgs.cryptsetup pkgs.btrfs-progs pkgs.util-linux pkgs.jq ];
        text = builtins.readFile ../scripts/braid-add-disk.sh;
      })
      (pkgs.writeShellApplication {
        name = "braid-remove-disk";
        runtimeInputs = [ pkgs.cryptsetup pkgs.btrfs-progs pkgs.util-linux pkgs.jq ];
        text = builtins.readFile ../scripts/braid-remove-disk.sh;
      })
      pkgs.cryptsetup
      pkgs.btrfs-progs
    ];

    environment.etc."braid/config.json".text = builtins.toJSON {
      disks = [
        "/dev/disk/by-id/virtio-disk1"
        "/dev/disk/by-id/virtio-disk2"
        "/dev/disk/by-id/virtio-disk3"
      ];
      mountPoint = "/mnt/storage";
    };
  };

  testScript = builtins.readFile ./braid-remove-disk.py;
}
