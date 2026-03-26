# Test: replace-new-already-luks
#
# What: New disk is already LUKS-formatted (but mapper not open and not in
# pool). Replace should open it and proceed without re-formatting.
#
# Why: Crash recovery path — if a previous replace crashed after LUKS format
# but before completing, retrying should not re-format the disk.
#
# Dependencies: braid add (builds the test pool), braid replace live path.
{ braid }:
{
  name = "replace-new-already-luks";

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

  testScript = builtins.readFile ./replace-new-already-luks.py;
}
