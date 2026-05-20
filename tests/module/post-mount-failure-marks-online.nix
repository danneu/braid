# Test: post-mount-failure-marks-online
#
# What: Bootstrap add failures after mount still activate braid-online.service,
# and ExecStop can lock the pool when pool.json is absent.
#
# Why: A post-mount error must not leave the pool mounted without the lifecycle
# owner, and shutdown cleanup must not depend on pool.json existing in the
# bootstrap failure window.
{ braid }:
{ pkgs, ... }:
{
  name = "post-mount-failure-marks-online";

  nodes.machine =
    { pkgs, ... }:
    {
      imports = [ ../../modules/braid ];

      braid = {
        enable = true;
        package = braid;
      };

      virtualisation.emptyDiskImages = [
        {
          size = 512;
          driveConfig.deviceExtraOpts.serial = "disk1";
        }
      ];
      virtualisation.memorySize = 2048;

      environment.systemPackages = [
        pkgs.btrfs-progs
        pkgs.cryptsetup
      ];
    };

  testScript = builtins.readFile ./post-mount-failure-marks-online.py;
}
