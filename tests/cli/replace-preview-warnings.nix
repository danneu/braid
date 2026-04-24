# Test: replace-preview-warnings
#
# What: Pins stdout/stderr routing for `braid replace` dry-run, `--yes`,
# and interactive keyfile diagnostics. Guards:
#
# 1. Live-path `braid replace --dry-run` prints the step preview on
#    stdout, includes keyfile PreviewNote warnings when relevant, and
#    keeps stderr empty.
# 2. `braid replace --yes` (no `--dry-run`) on a pool with keyfile
#    probe failures renders canonical `[warn]` notes on stderr, not
#    stdout, before mutating.
# 3. Missing-path `braid replace --dry-run` prints the missing-path
#    step preview on stdout and keeps stderr empty.
# 4. Failed keyfile-enrollment probes become PreviewNote warnings when
#    no existing member proves keyslot 1 is occupied.
# 5. Proved keyfile enrollment suppresses redundant probe-failure
#    warnings and emits the keyfile-asymmetry warning instead.
#
# Why: PR 8 routes replace's `--dry-run` through `Preview::render` via
# `ReplacePlan::preview()`. Keyfile-asymmetry and keyfile-probe
# uncertainty are plan diagnostics now, so dry-run stdout, real-run
# stderr, and interactive stderr must share the same `PreviewNote::Warn`
# wording. A regression that (a) leaked dry-run diagnostics to stderr,
# (b) revived the legacy `WARNING:` or `note:` keyfile strings, (c)
# suppressed warnings during `--yes`, or (d) printed redundant probe
# notes after another member proves enrollment would slip past the other
# replace VM tests.
#
# Scenario: operator builds a 2-disk RAID1 pool with a keyfile enrolled
# for auto-unlock. Operator then tries `braid replace` with a fresh
# (non-LUKS) disk, without specifying `--enroll` for the new disk. The
# keyfile-asymmetry and probe-failure warnings must stay on the command's
# owned stream for each mode, and aborting interactively with `no` must
# not mutate the pool.
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
