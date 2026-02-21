# Test: braid-add-disk
#
# What: Runs the braid-add-disk script through its full lifecycle: first disk
# (creates pool), second disk (converts to RAID1), third disk (expands pool),
# plus validation errors, crash recovery, and unmounted pool guard.
#
# Why: This is the first real deliverable — the one imperative command that
# orchestrates LUKS + btrfs for new disks. Every primitive has been proven in
# isolation (luks, btrfs-raid1, grow, shrink, heal, degrade). This test proves
# the script ties them together correctly.
#
# Dependencies: btrfs-grow1 (single -> RAID1 -> 3-drive works manually).
{
  name = "braid-add-disk";

  nodes.machine = { pkgs, ... }: {
    virtualisation.emptyDiskImages = [
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk2"; }
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk3"; }
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk4"; }
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk5"; }
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
