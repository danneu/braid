# Test: braid recover
#
# What: Verifies `braid recover` can self-mount the pool and rebuild
# pool.json from live state when recovering from an interrupted operation.
#
# Why: After an interrupted mutation, `pending-op.json` blocks `braid unlock`.
# `braid recover` must be able to open LUKS and mount the pool itself,
# rather than requiring manual cryptsetup + mount commands.
{ braid }:
{
  name = "braid-recover";

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

  testScript = builtins.readFile ./braid-recover.py;
}
