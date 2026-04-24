# Test: replace-preview-warnings
#
# What: Pins stdout/stderr routing for `braid replace` dry-run, `--yes`,
# and interactive confirmation keyfile diagnostics. Guards:
#
# 1. Live-path `braid replace --dry-run` prints the step preview on
#    stdout and keeps stderr empty.
# 2. `braid replace --yes` (no `--dry-run`) on a pool with keyfile
#    enrollment and a fresh (non-LUKS) replacement disk does NOT emit
#    the keyfile-asymmetry `WARNING:` block on stdout or stderr.
# 3. Missing-path `braid replace --dry-run` prints the missing-path
#    step preview on stdout and keeps stderr empty.
# 4. Failed keyfile-enrollment probes stay quiet for dry-run and `--yes`.
# 5. Interactive confirmation prints probe-failure notes only when no
#    existing member proves keyslot 1 is occupied.
#
# Why: PR 8 routes replace's `--dry-run` through `Preview::render` via
# `ReplacePlan::preview()`. The keyfile-asymmetry warning is
# deliberately NOT a `PreviewNote` -- it stays inside the `!params.yes`
# confirmation gate in `execute()`. Probe failures are structured data
# routed by the caller: they should be visible only in that interactive
# confirmation path when enrollment cannot be proven. A regression that
# (a) leaked probe events or banners to stderr during dry-run, (b)
# widened the confirmation-only warning into a `PreviewNote::Warn`, (c)
# dropped the `!params.yes` gate, or (d) printed redundant probe notes
# after another member proves enrollment would slip past the other
# replace VM tests.
#
# Scenario: operator builds a 2-disk RAID1 pool with a keyfile enrolled
# for auto-unlock. Operator then tries `braid replace` with a fresh
# (non-LUKS) disk, without specifying `--enroll` for the new disk. The
# keyfile-asymmetry `WARNING:` and probe-failure notes must not appear
# on `--dry-run` or `--yes`; interactive confirmation owns those
# diagnostics and aborting with `no` must not mutate the pool.
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
