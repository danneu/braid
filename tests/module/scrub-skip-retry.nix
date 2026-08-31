# Test: scrub-skip-retry
#
# What: Verifies end to end that a scheduled scrub which fires while braid is
# busy with the pool skips (exit 4) instead of running or failing, that the skip
# raises no alert -- including when the retry wait is stopped -- that the retry
# runs the scrub for real once the pool is clear, and that a deferral surviving
# in /var/lib/braid makes the pool-online trigger re-poke the service.
#
# Why: the caja incident (2026-08-31) had a `braid add` convert balance
# mid-flight with the monthly scrub due at midnight, and nothing stopped the
# scrub from piling onto the same spindles; a scrub firing during a `btrfs
# replace` is worse still -- kernel-rejected, exit 1, spurious alert. Unit tests
# pin the gate's decisions, but only a real paused balance plus real systemd
# proves the exit-4/SuccessExitStatus/RestartForceExitStatus wiring actually
# retries instead of alerting or giving up.
#
# Scenario: One node with a 2-disk RAID1 pool, monitor enabled, and a 5s retry
# interval. An operator's convert balance is paused overnight when the monthly
# scrub is due; the scrub defers, retries, and only runs once the balance is
# gone -- and after a stop-and-reboot-shaped interruption the pool-online
# trigger still owes it.
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
        # Seconds, not the 1h default: the test has to observe a real retry.
        autoScrub.retryInterval = "5s";
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
    builtins.readFile ./balance_helpers.py
    + "\n\n"
    + builtins.readFile ./scrub-skip-retry.py;
}
