# Test: braid apply
#
# What: Exercises the apply engine across add, remove, replace, checkpoint/resume,
# stale checkpoint rejection, redundancy confirmation, and no-op scenarios.
#
# Why: The apply engine executes destructive operations. It must checkpoint correctly,
# resume safely, and refuse stale state.
#
# Dependencies: braid plan (Phase 1), braid init-disk + braid apply (pool setup).
{
  name = "braid-apply";

  nodes.machine = { pkgs, ... }: {
    virtualisation.emptyDiskImages = [
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk2"; }
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk3"; }
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk4"; }
    ];

    environment.systemPackages = let
      braid-cli = pkgs.writeShellApplication {
        name = "braid";
        runtimeInputs = [ pkgs.cryptsetup pkgs.btrfs-progs pkgs.util-linux pkgs.jq pkgs.coreutils ];
        text = builtins.readFile ../scripts/braid.sh;
      };
    in [
      braid-cli
      pkgs.cryptsetup
      pkgs.btrfs-progs
      pkgs.jq
    ];

    environment.etc."braid/config.json".text = builtins.toJSON {
      disks = [
        "/dev/disk/by-id/virtio-disk1"
        "/dev/disk/by-id/virtio-disk2"
        "/dev/disk/by-id/virtio-disk3"
        "/dev/disk/by-id/virtio-disk4"
      ];
      mountPoint = "/mnt/storage";
    };
  };

  testScript = builtins.readFile ./braid-apply.py;
}
