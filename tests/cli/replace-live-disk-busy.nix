# Test: replace-live-disk-busy
#
# What: Runs `braid replace` for a live disk while a loop device holds the
# old mapper busy, so the post-replace `cryptsetup close` fails with EBUSY.
# Verifies the command still exits 0, prints the close-failure warning, and
# does NOT print the contradictory "Old device closed" follow-up.
#
# Why: Guards against the regression where the wipe-guidance line printed
# unconditionally after a failed best-effort close, which was both
# contradictory and actively dangerous (operator could wipe a disk whose
# mapper was still open on live data).
#
# Dependencies: braid add (builds the test pool), braid replace live path.
{ braid }:
{
  name = "replace-live-disk-busy";

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

  testScript = builtins.readFile ./replace-live-disk-busy.py;
}
