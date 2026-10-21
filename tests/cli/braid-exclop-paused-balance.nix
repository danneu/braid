# Test: braid add refuses and braid lock proceeds when a balance is paused
#
# What: Validates that `braid add` fails fast when a balance is paused,
# without hanging, and that `braid lock` unmounts under it, warns, and leaves
# it paused for the next unlock to report.
#
# Why: A paused balance holds the exclusive lock indefinitely. --enqueue would
# hang forever waiting for it to clear, so add must detect this via sysfs and
# error immediately with an actionable message. Lock used to refuse too, with
# advice to wait for a balance that never finishes on its own. Unmount keeps
# the balance paused, so lock now proceeds.
#
# How: Creates a 2-disk pool, writes data, starts and pauses a balance (via
# the shared balance_helpers retry pattern), verifies that braid add refuses,
# then locks, unlocks, and checks the balance is still paused.
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

  testScript =
    builtins.readFile ./../module/balance_helpers.py
    + "\n\n"
    + builtins.readFile ./braid-exclop-paused-balance.py;
}
