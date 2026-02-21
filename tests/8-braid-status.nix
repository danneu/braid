# Test: braid-status
#
# What: Runs braid-status in summary and verbose modes against a healthy
# 3-disk RAID1 pool, then simulates a drive failure and verifies degraded
# output. Also tests error on unmounted pool.
#
# Why: braid-status is the operator's primary diagnostic tool. It reads
# live btrfs/LUKS state, so it must be tested in a real VM with real
# filesystems to validate parsing of actual command output.
#
# Dependencies: braid-add-disk (pool creation).
{
  name = "braid-status";

  nodes.machine = { pkgs, ... }: {
    virtualisation.emptyDiskImages = [
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk2"; }
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk3"; }
    ];

    environment.systemPackages = let
      braid-cli = pkgs.writeShellApplication {
        name = "braid";
        runtimeInputs = [ pkgs.cryptsetup pkgs.btrfs-progs pkgs.util-linux pkgs.jq pkgs.coreutils ];
        text = builtins.readFile ../scripts/braid.sh;
      };
    in [
      braid-cli
      (pkgs.writeShellApplication {
        name = "braid-add-disk";
        runtimeInputs = [ braid-cli pkgs.cryptsetup pkgs.btrfs-progs pkgs.util-linux pkgs.jq ];
        text = builtins.readFile ../scripts/braid-add-disk.sh;
      })
      (pkgs.writeShellApplication {
        name = "braid-status";
        runtimeInputs = [ pkgs.cryptsetup pkgs.btrfs-progs pkgs.util-linux pkgs.jq ];
        text = builtins.readFile ../scripts/braid-status.sh;
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

  testScript = builtins.readFile ./braid-status.py;
}
