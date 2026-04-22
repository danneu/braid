# Test: braid lock
#
# What: Verifies `braid lock` unmounts the pool and closes LUKS volumes.
#
# Why: There is no inverse of `braid unlock` — users must manually umount +
# cryptsetup close each mapper. This tests the happy path, idempotency,
# partial state recovery, and round-trip with unlock.
{ braid }:
{
  name = "braid-lock";

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

  testScript = builtins.readFile ./braid-lock.py;
}
