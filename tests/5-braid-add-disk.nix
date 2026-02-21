# Test: braid init-disk + apply lifecycle
#
# What: Runs `braid init-disk` + `braid apply` through its full lifecycle: first
# disk (creates pool), second disk (converts to RAID1), third disk (expands pool),
# plus validation errors, crash recovery, and unmounted pool guard.
#
# Why: This is the first real deliverable — the unified CLI that orchestrates
# LUKS + btrfs for new disks. Every primitive has been proven in isolation
# (luks, btrfs-raid1, grow, shrink, heal, degrade). This test proves the
# unified CLI ties them together correctly.
#
# Dependencies: btrfs-grow1 (single -> RAID1 -> 3-drive works manually).
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
    ];

    environment.etc."braid/config.json".text = builtins.toJSON {
      disks = [
        "/dev/disk/by-id/virtio-disk1"
        "/dev/disk/by-id/virtio-disk2"
        "/dev/disk/by-id/virtio-disk3"
        "/dev/disk/by-id/virtio-disk4"
        "/dev/disk/by-id/virtio-disk5"
      ];
      mountPoint = "/mnt/storage";
    };
  };

  testScript = builtins.readFile ./braid-add-disk.py;
}
