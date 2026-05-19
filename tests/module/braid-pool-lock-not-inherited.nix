# Test: braid-pool-lock-not-inherited
#
# What: While `braid add` is in flight, only the braid binary holds an fd on
# /run/braid-pool.lock; descendants do not inherit it.
#
# Why: Rust opens the lock with O_CLOEXEC. A regression that leaks the fd into
# systemd-inhibit or its children would keep the advisory flock alive past the
# braid process lifetime.
{ braid }:
{
  name = "braid-pool-lock-not-inherited";

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
          size = 2048;
          driveConfig.deviceExtraOpts.serial = "disk1";
        }
        {
          size = 2048;
          driveConfig.deviceExtraOpts.serial = "disk2";
        }
      ];
      virtualisation.memorySize = 1024;

      environment.systemPackages = [
        pkgs.cryptsetup
        pkgs.btrfs-progs
      ];
    };

  testScript =
    builtins.readFile ../cli/inhibitor_helpers.py
    + "\n\n"
    + builtins.readFile ./braid-pool-lock-not-inherited.py;
}
