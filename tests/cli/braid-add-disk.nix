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
      mount_point = "/mnt/storage";
    };
  };

  testScript = builtins.readFile ./braid-add-disk.py;
}
