# Test: braid commands refuse when balance is paused
#
# What: Validates that `braid add` and `braid lock` both fail fast when a
# balance is paused, without hanging or proceeding unsafely.
#
# Why: A paused balance holds the exclusive lock indefinitely. --enqueue would
# hang forever waiting for it to clear. Braid must detect this via sysfs and
# error immediately with an actionable message.
#
# How: Creates a 2-disk pool, writes data, starts and pauses a balance (using
# the retry pattern from braid-status-during-balance), then verifies that
# braid add and braid lock both refuse.
#
# Dependencies: Rust braid binary for all commands.
{ braid }:
{
  name = "braid-exclop-paused-balance";

  nodes.machine =
    { pkgs, ... }:
    {
      virtualisation.emptyDiskImages = [
        {
          size = 4096;
          driveConfig.deviceExtraOpts.serial = "disk1";
        }
        {
          size = 4096;
          driveConfig.deviceExtraOpts.serial = "disk2";
        }
        {
          size = 4096;
          driveConfig.deviceExtraOpts.serial = "disk3";
        }
      ];

      virtualisation.memorySize = 2048;

      environment.systemPackages = [
        braid
        pkgs.cryptsetup
        pkgs.btrfs-progs
      ];

      environment.etc."braid/config.json".text = builtins.toJSON {
        mount_point = "/mnt/storage";
      };
    };

  testScript = builtins.readFile ./braid-exclop-paused-balance.py;
}
