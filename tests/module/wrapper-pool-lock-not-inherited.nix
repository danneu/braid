# Test: wrapper-pool-lock-not-inherited
#
# What: While `braid add` is in flight, ONLY the wrapper bash holds
# fd 9 on /run/braid-pool.lock. The braid binary and every
# descendant (notably systemd-inhibit and its sh+sleep child) must
# NOT have fd 9 inherited.
#
# Why: Defense-in-depth structural check for the same bug as
# wrapper-pool-lock-released-after-sigkill.py. That test fires only
# on the SIGKILL path; this one catches inheritance regressions even
# when the orphan path doesn't trigger in a given run, by directly
# asserting on /proc/<pid>/fd shapes during normal operation.
# Timing-independent: no kill needed, no race window.
#
# Same module-import wiring as the SIGKILL test -- `braid` on PATH
# must be the wrapper script; with the wrong wiring fd 9 is never
# opened and the assertion is vacuous.
{ braid }:
{
  name = "wrapper-pool-lock-not-inherited";

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
    + builtins.readFile ./wrapper-pool-lock-not-inherited.py;
}
