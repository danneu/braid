# Test: braid unified CLI
#
# What: Exercises `braid status` (human, --json), verifies the full add
# workflow, and validates error cases.
#
# Why: The unified CLI must produce correct results for status reporting after
# pool setup using named disk commands.
#
# Dependencies: braid add (pool setup).
{ braid }:
{
  name = "braid-unified";

  nodes.machine = { pkgs, ... }: {
    virtualisation.emptyDiskImages = [
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk2"; }
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk3"; }
    ];

    environment.systemPackages = [
      braid
      pkgs.cryptsetup
      pkgs.btrfs-progs
      pkgs.jq
    ];

    environment.etc."braid/config.json".text = builtins.toJSON {
      mount_point = "/mnt/storage";
    };
  };

  testScript = builtins.readFile ./braid-unified.py;
}
