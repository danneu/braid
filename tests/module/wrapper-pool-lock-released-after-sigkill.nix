# Test: wrapper-pool-lock-released-after-sigkill
#
# What: When the braid binary is SIGKILL'd mid-`braid add`, the pool
# lock at /run/braid-pool.lock must be released within seconds. The
# next attempt to acquire the flock must succeed.
#
# Why: braid-wrapper.sh opens fd 9 on /run/braid-pool.lock and
# acquires an advisory flock. fd 9 has no FD_CLOEXEC; without the
# `9>&-` redirect on the braid invocation, fd 9 is inherited by the
# braid binary and by any descendant it spawns -- notably
# systemd-inhibit (cli/src/inhibit.rs), which is in its own pgroup
# (process_group(0)) and survives SIGKILL/OOM/SIGTERM of braid. The
# orphan keeps fd 9 alive past the wrapper's exit; flock is held on
# the open file description, so the lock stays held forever.
#
# This test must use the module-import wiring (imports =
# [ ../../modules/braid ]) so `braid` on PATH is the wrapper script
# from modules/braid/braid-wrapper.sh, NOT the flake-level
# linuxCrane.braid (which is plain makeWrapper-on-PATH and never
# touches the wrapper script). With the wrong wiring fd 9 is never
# opened, the regression assertion vacuously passes, and the test
# loses its bug-locking value.
{ braid }:
{
  name = "wrapper-pool-lock-released-after-sigkill";

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

  # Inhibitor query helpers (list_inhibitors / find_braid_sleep_inhibitor)
  # are concatenated into the test script's global namespace at Nix-eval
  # time -- the NixOS test driver runs testScript as a single Python
  # string with no module path, so a normal `import` would not work.
  # See tests/cli/inhibitor_helpers.py for details.
  testScript =
    builtins.readFile ../cli/inhibitor_helpers.py
    + "\n\n"
    + builtins.readFile ./wrapper-pool-lock-released-after-sigkill.py;
}
