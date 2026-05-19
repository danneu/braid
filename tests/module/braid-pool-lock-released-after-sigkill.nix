# Test: braid-pool-lock-released-after-sigkill
#
# What: SIGKILL of the braid binary releases /run/braid-pool.lock and the next
# non-blocking flock succeeds.
#
# Why: The Rust lock fd is O_CLOEXEC and owned by the braid process. Descendant
# inheritance would leave false contention after process death.
{ braid }:
{
  name = "braid-pool-lock-released-after-sigkill";

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
    + builtins.readFile ./braid-pool-lock-released-after-sigkill.py;
}
