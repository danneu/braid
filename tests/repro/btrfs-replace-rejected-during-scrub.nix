# Repro: btrfs replace start is rejected when a scrub is in progress
#
# Locks the upstream stderr wording the `replace_error` classifier in
# `cli/src/pool.rs` matches against. Scrub is not part of btrfs'
# `exclusive_operation` set, so the `--enqueue` flag braid passes does
# NOT wait scrub out -- the kernel returns
# BTRFS_IOCTL_DEV_REPLACE_RESULT_SCRUB_INPROGRESS and `btrfs replace start`
# emits "scrub is in progress" in stderr. A `nixpkgs`-bump-induced
# wording shift fails this test loudly before the unit-level classifier
# can silently misclassify in production.
#
# Three 1024 MiB disks: disk1 + disk2 form an unencrypted RAID1 pool, disk3
# is the standby replacement target. The live-scrub window is deterministic:
# the shared throttle helper caps each device at 20 MiB/s via the
# scrub_speed_max knob, so a 400 MiB payload scrubs for ~20 seconds (see
# `tests/repro/btrfs-scrub-limit-bounds-rate.nix` for the behavior lock the
# throttle rests on).
{
  name = "repro-btrfs-replace-rejected-during-scrub";

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

      environment.systemPackages = [ pkgs.btrfs-progs ];
    };

  testScript =
    builtins.readFile ./scrub_throttle_helpers.py
    + "\n\n"
    + builtins.readFile ./btrfs-replace-rejected-during-scrub.py;
}
