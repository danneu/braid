# Test: braid lock orphan mapper cleanup
#
# What: Verifies `braid lock` closes orphaned braid-* mappers that exist in
# /dev/mapper but are not listed in pool.json (crash window simulation).
{ braid }:
{
  name = "braid-lock-orphan";

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
    ];

    environment.etc."braid/config.json".text = builtins.toJSON {
      mount_point = "/mnt/storage";
    };
  };

  testScript = builtins.readFile ./braid-lock-orphan.py;
}
