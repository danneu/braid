# Test: braid remove -- ENOSPC pre-flight soft-warn stream routing
#
# What: Verifies the stream-routing contract for the soft-warn path of
# `braid remove`'s ENOSPC eviction-space pre-flight on the `remaining
# >= 2` branch. When the pre-flight check itself fails (command error
# or non-`CommandFailed` parse error), the plan must still proceed
# with a warning -- that warning lands on stdout as a `[warn] ...`
# note under `--dry-run`, and on stderr as the canonical `[warn] ...`
# line under real-run.
#
# Why: PR 4 of the project-wide Preview migration moves the soft-warn
# from a direct `eprintln!` in `check_eviction_space` to a
# `PreviewNote::Warn`. Without this test, a regression that either
# (a) prints the warning directly to stderr during `--dry-run`, or
# (b) drops the `[warn] ` prefix during real-run, would only
# be caught by a human noticing drift in their SSH session.
#
# The test forces the soft-warn branch by wrapping `btrfs` on PATH
# with a counter-based shim that errors on every `btrfs device usage
# --raw` call -- `braid remove` only invokes that request once, from
# `check_eviction_space`, so the first-call failure is enough.
#
# Dependencies: braid add (builds the test pool).
{ braid }:
{
  name = "braid-remove-softwarn";

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
        {
          size = 1024;
          driveConfig.deviceExtraOpts.serial = "disk3";
        }
      ];

      environment.systemPackages = [
        braid
        pkgs.cryptsetup
        pkgs.btrfs-progs
      ];

      environment.etc."braid/config.json".text = builtins.toJSON {
        mount_point = "/mnt/storage";
      };
    };

  testScript = builtins.readFile ./braid-remove-softwarn.py;
}
