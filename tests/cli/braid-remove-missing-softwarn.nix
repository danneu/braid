# Test: braid remove-missing -- ENOSPC pre-flight soft-warn stream routing
#
# What: Verifies the stream-routing contract for the soft-warn path of
# `braid remove-missing`'s ENOSPC relocation-space pre-flight. When the
# pre-flight check itself fails (command error or parse error), the plan
# must still proceed with a warning -- but that warning lands on stdout
# as a `[warn] ...` note under `--dry-run`, and on stderr as the canonical
# `[warn] ...` line under real-run.
#
# Why: PR 3 of the project-wide Preview migration moves the soft-warn
# from a direct `eprintln!` in `check_relocation_space` to a
# `PreviewNote::Warn`. Without this test, a regression that either
# (a) prints the warning directly to stderr during `--dry-run`, or
# (b) drops the `[warn] ` prefix during real-run, would only be
# caught by a human noticing drift in their SSH session.
#
# The test forces the soft-warn branch by wrapping `btrfs` on PATH with
# a counter-based shim that delegates the first `btrfs device usage
# --raw` call (used by `probe_missing_devids` to validate the
# --missing-id argument) to the real binary, but errors on every
# subsequent call (which lands in `check_relocation_space`).
#
# Dependencies: braid add (builds the test pool).
{ braid }:
{
  name = "braid-remove-missing-softwarn";

  nodes.machine =
    { pkgs, ... }:
    {
      virtualisation.emptyDiskImages = [
        {
          size = 512;
          driveConfig.deviceExtraOpts.serial = "disk1";
        }
        {
          size = 512;
          driveConfig.deviceExtraOpts.serial = "disk2";
        }
        {
          size = 512;
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

  testScript = builtins.readFile ./braid-remove-missing-softwarn.py;
}
