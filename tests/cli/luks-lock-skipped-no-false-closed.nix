# Test: luks-lock-skipped-no-false-closed
#
# What: Runs `braid lock` with a bogus braid-prefixed mapper entry that
# cannot be classified by cryptsetup.
#
# Why: The lock prelude must not turn skipped mapper uncertainty into false
# "already closed" rows for members that might be live under a drifted name.
#
# Dependencies: Rust braid binary, btrfs-progs, cryptsetup, and two virtual
# disks for building a small encrypted btrfs pool.
{ braid }:
{
  name = "luks-lock-skipped-no-false-closed";

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
        pkgs.btrfs-progs
        pkgs.cryptsetup
      ];

      environment.etc."braid/config.json".text = builtins.toJSON {
        mount_point = "/mnt/storage";
      };
    };

  testScript = builtins.readFile ./luks-lock-skipped-no-false-closed.py;
}
