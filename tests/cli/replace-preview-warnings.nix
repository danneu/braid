# Test: replace-preview-warnings
#
# What: Pins stdout/stderr routing for `braid replace` dry-run and the
# `--yes` real-run no-leak contract for the keyfile-asymmetry
# `WARNING:` block after the PR 8 Preview migration. Three guards:
#
# 1. Live-path `braid replace --dry-run` prints the step preview on
#    stdout and keeps stderr empty.
# 2. `braid replace --yes` (no `--dry-run`) on a pool with keyfile
#    enrollment and a fresh (non-LUKS) replacement disk does NOT emit
#    the keyfile-asymmetry `WARNING:` block on stdout or stderr.
# 3. Missing-path `braid replace --dry-run` prints the missing-path
#    step preview on stdout and keeps stderr empty.
#
# Why: PR 8 routes replace's `--dry-run` through `Preview::render` via
# `ReplacePlan::preview()`. The keyfile-asymmetry warning is
# deliberately NOT a `PreviewNote` -- it stays inside the `!params.yes`
# confirmation gate in `execute()`. A regression that (a) leaked probe
# events or banners to stderr during dry-run, (b) widened the
# confirmation-only warning into a `PreviewNote::Warn`, or (c) dropped
# the `!params.yes` gate would slip past the other replace VM tests.
#
# Scenario: operator builds a 2-disk RAID1 pool with a keyfile enrolled
# for auto-unlock. Operator then tries `braid replace` with a fresh
# (non-LUKS) disk, without specifying `--enroll` for the new disk. The
# keyfile-asymmetry `WARNING:` must not appear on `--dry-run` or
# `--yes` real-run; it may only appear in the interactive confirmation
# path (covered by existing VM tests).
{ braid }:
{
  name = "replace-preview-warnings";

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
        {
          size = 1024;
          driveConfig.deviceExtraOpts.serial = "disk4";
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

  testScript = builtins.readFile ./replace-preview-warnings.py;
}
