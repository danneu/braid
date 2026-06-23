# Test: replace-live-disk
#
# What: Runs `braid replace --old <live> --new <new>` to replace a live,
# present disk in a healthy pool in place with a single `btrfs replace start`,
# closing the old disk's LUKS mapper once the replace completes and leaving the
# pool healthy and redundant. Also covers the in-step `--enroll` keyfile path
# and the live-path guards that reject `--missing-id` and a degraded pool.
#
# Why: Before this feature, replacing a live disk meant orchestrating a
# separate `braid remove` + `braid add` by hand. The unified `braid replace`
# swaps the disk in place with `btrfs replace start` -- one operator step, and
# the source stays in the pool until the copy completes, so the array is never
# degraded mid-swap.
#
# Dependencies: braid add (builds the test pool), braid replace live path.
{ braid }:
{
  name = "replace-live-disk";

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
        {
          size = 1024;
          driveConfig.deviceExtraOpts.serial = "disk4";
        }
        {
          size = 1024;
          driveConfig.deviceExtraOpts.serial = "disk5";
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

  testScript =
    builtins.readFile ./member_helpers.py
    + "\n\n"
    + builtins.readFile ./replace-live-disk.py;
}
