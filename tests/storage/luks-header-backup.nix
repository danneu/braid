# Test: LUKS header auto-backup on braid add, corrupt header restore + data recovery
#
# What: Verifies that braid add automatically backs up LUKS headers,
# and that a corrupted header can be restored from backup to recover data.
#
# Why: LUKS header corruption means permanent data loss regardless of knowing
# the passphrase. braid add is the only luksFormat path (Principle 3), so
# auto-backup here guarantees every formatted disk has a recoverable header.
#
# Dependencies: LUKS primitives, btrfs basics, Rust braid binary with add command.
{ braid }:
{
  name = "luks-header-backup";

  nodes.machine =
    { pkgs, ... }:
    {
      virtualisation.emptyDiskImages = [
        {
          size = 256;
          driveConfig.deviceExtraOpts.serial = "disk1";
        }
        {
          size = 256;
          driveConfig.deviceExtraOpts.serial = "disk2";
        }
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

  testScript = builtins.readFile ./luks-header-backup.py;
}
