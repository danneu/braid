# Test: replace-passphrase-mismatch
#
# What: Wrong passphrase is rejected before any destructive action (LUKS
# format) is taken on the new disk.
#
# Why: If passphrase verification happened after formatting, the new disk
# would be LUKS-formatted with a passphrase that doesn't match the pool,
# creating an inaccessible disk.
#
# Dependencies: braid add (builds the test pool), braid replace validation.
{ braid }:
{
  name = "replace-passphrase-mismatch";

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

  testScript = builtins.readFile ./replace-passphrase-mismatch.py;
}
