# Test: braid lock — umount busy gives actionable error
#
# What: When umount fails because a process holds a file open, `braid lock`
# fails with an actionable error message that mentions `lsof` or `fuser`.
#
# Why: The raw umount stderr is not actionable for users who don't know how
# to debug "target is busy". The hint tells them exactly what to do.
{ braid }:
{
  name = "braid-lock-umount-busy";

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
        pkgs.lsof
      ];

      environment.etc."braid/config.json".text = builtins.toJSON {
        mount_point = "/mnt/storage";
      };
    };

  testScript = builtins.readFile ./braid-lock-umount-busy.py;
}
