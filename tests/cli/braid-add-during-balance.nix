# Test: braid add waits for in-flight balance via --enqueue
#
# What: Validates that `braid add` detects an active exclusive op via sysfs,
# prints a "waiting" message, and succeeds after the op finishes (via --enqueue).
#
# Why: The exclusive op preflight was migrated from parsing `btrfs balance status`
# to reading /sys/fs/btrfs/{fsid}/exclusive_operation. This test proves the full
# end-to-end path: sysfs read → wait message → --enqueue blocks → balance
# finishes → device add succeeds.
#
# How: Creates a 2-disk pool, writes data, starts a background balance, waits
# until the balance is confirmed running, then runs `braid add disk3`. The add
# should wait (via --enqueue) and succeed once the balance completes.
#
# Dependencies: Rust braid binary for all commands.
{ braid }:
{
  name = "braid-add-during-balance";

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

  testScript = builtins.readFile ./braid-add-during-balance.py;
}
