# Test: alert-state-lock
#
# What: Verifies that every alert-state mutator acquires the Rust-owned
# `/run/braid-pool.lock` before it can mutate alert-latch.json or
# acked-stats.json.
#
# Why: monitor, ack, add, remove, and remove-missing all touch alert
# state. The pool lock is the serialization boundary that prevents
# stale read-modify-write cycles from resurrecting acknowledged alerts.
# See docs/design/decisions/026-pool-lock-rust-owned.md.
{ braid }:
{ pkgs, ... }:
{
  name = "alert-state-lock";

  nodes.machine =
    { pkgs, ... }:
    {
      imports = [ ../../modules/braid ];

      braid = {
        enable = true;
        package = braid;
        monitor = {
          beep = false;
          interval = "1h";
        };
      };

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
      virtualisation.memorySize = 2048;

      environment.systemPackages = [
        pkgs.btrfs-progs
        pkgs.cryptsetup
        pkgs.util-linux
      ];
    };

  testScript = builtins.readFile ./alert-state-lock.py;
}
