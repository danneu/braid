# Myth-bust: "second degraded mount goes read-only" (DISPROVED)
#
# Pre-kernel 4.14 (Oct 2017), btrfs checked degraded viability at the device
# level — any missing device + any single-profile chunk (tolerance=0) caused
# the rw mount to be refused. Since 4.14, the check is per-chunk: single
# chunks created during degraded operation live on the AVAILABLE device, so
# missing=0 and the check passes.
#
# This test confirms the modern behavior: a second degraded mount still comes
# up rw. However, it also proves the REAL risk: data written while degraded
# gets single-profile chunks with NO redundancy. braid must detect and warn
# about this.
{
  name = "repro-degrade2x-read-only";

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
        pkgs.cryptsetup
        pkgs.btrfs-progs
      ];
    };

  testScript = builtins.readFile ./degrade2x-read-only.py;
}
