# Test: scrub-skip-retry
#
# What: Verifies end to end that a scheduled scrub which fires while braid is
# busy with the pool skips (exit 4) instead of running or failing, that the skip
# raises no alert, that the *next poll* runs the scrub for real once the pool is
# clear, and that a poll on the now-fresh pool is a no-op that touches nothing.
#
# Why: the caja incident (2026-08-31) had a `braid add` convert balance
# mid-flight with the monthly scrub due at midnight, and nothing stopped the
# scrub from piling onto the same spindles; a scrub firing during a `btrfs
# replace` is worse still -- kernel-rejected, exit 1, spurious alert. Unit tests
# pin the gate's decisions, but only a real paused balance plus real systemd
# proves that exit 4 stays off onFailure and that the retry now comes from the
# timer's next poll rather than a unit-level restart -- the arc this design
# deleted along with RestartForceExitStatus, RestartSec, and the deferred flag.
#
# Scenario: One node with a 2-disk RAID1 pool and monitor enabled. An operator's
# convert balance is paused overnight when the scrub comes due; the scrub skips,
# the next poll after the balance is gone runs it for real, and the poll after
# that finds the pool fresh and does nothing.
{ braid }:
{ ... }:
{
  name = "scrub-skip-retry";

  nodes.busy =
    { pkgs, ... }:
    {
      imports = [ ../../modules/braid ];

      braid = {
        enable = true;
        package = braid;
        autoScrub.enable = true;
        monitor.enable = true;
        monitor.alertCommand = "touch /root/alert-fired";
      };

      virtualisation.emptyDiskImages = [
        {
          size = 4096;
          driveConfig.deviceExtraOpts.serial = "disk1";
        }
        {
          size = 4096;
          driveConfig.deviceExtraOpts.serial = "disk2";
        }
      ];
      virtualisation.memorySize = 2048;

      environment.systemPackages = [
        pkgs.btrfs-progs
        pkgs.cryptsetup
      ];
    };

  testScript =
    builtins.readFile ./balance_helpers.py + "\n\n" + builtins.readFile ./scrub-skip-retry.py;
}
