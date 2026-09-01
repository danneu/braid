# Repro: btrfs scrub start/resume are rejected while a scrub is already running
#
# Locks the upstream stderr wording that `classify_btrfs_failure` in
# `cli/src/scrub_resume_or_start.rs` matches to tell an invocation-time
# collision -- an external scrub that started after braid's gate probe cleared
# -- from a genuine scrub failure. The collision is a busy skip (exit 4, no
# alert); everything else beeps. A `nixpkgs`-bump-induced wording shift fails
# this test loudly before the unit-level classifier silently turns every lost
# race back into a spurious 3am alert.
#
# Two 1024 MiB disks form an unencrypted btrfs RAID1 pool. The live-scrub
# window is deterministic: the shared throttle helper caps each device at
# 20 MiB/s via the scrub_speed_max knob, so a 400 MiB payload scrubs for ~20
# seconds (see `tests/repro/btrfs-scrub-limit-bounds-rate.nix` for the
# behavior lock the throttle rests on).
{
  name = "repro-btrfs-scrub-start-rejected-during-scrub";

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
      ];

      environment.systemPackages = [ pkgs.btrfs-progs ];
    };

  testScript =
    builtins.readFile ./scrub_throttle_helpers.py
    + "\n\n"
    + builtins.readFile ./btrfs-scrub-start-rejected-during-scrub.py;
}
