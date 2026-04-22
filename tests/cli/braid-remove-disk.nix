# Test: braid remove
#
# What: Runs `braid remove <name>` through its lifecycle: graceful remove,
# remove-missing, LUKS cleanup, redundancy warning, and validation errors.
#
# Why: Symmetric counterpart to braid add. Must handle both happy path (disk
# present, data migrates off) and failure path (disk gone, remove missing).
#
# Dependencies: braid add (builds the test pool).
{ braid }:
{
  name = "braid-remove-disk";

  nodes.machine =
    { pkgs, ... }:
    {
      virtualisation.emptyDiskImages = [
        {
          size = 1024;
          driveConfig.deviceExtraOpts.serial = "disk1";
        }
        {
          size = 1024;
          driveConfig.deviceExtraOpts.serial = "disk2";
        }
        {
          size = 1024;
          driveConfig.deviceExtraOpts.serial = "disk3";
        }
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

  testScript = builtins.readFile ./braid-remove-disk.py;
}
