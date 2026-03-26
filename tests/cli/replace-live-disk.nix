# Test: replace-live-disk
#
# What: Runs `braid replace --old <live> --new <new>` to replace a live,
# present disk in a healthy pool. Validates the add-first ordering, data
# integrity, and that the old disk is fully evicted (removed + LUKS closed).
# Also covers mixed-state rejection and --missing-id rejection on live path.
#
# Why: Before this feature, replacing a live disk required separate
# `braid remove` + `braid add`. The unified `braid replace` preserves
# add-first ordering (redundancy never drops) and is simpler for operators.
#
# Dependencies: braid add (builds the test pool), braid replace live path.
{ braid }:
{
  name = "replace-live-disk";

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
      mount_point = "/mnt/storage";
    };
  };

  testScript = builtins.readFile ./replace-live-disk.py;
}
