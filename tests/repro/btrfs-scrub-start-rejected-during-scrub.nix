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
# Two 4096 MiB disks form the LUKS + btrfs RAID1 pool, sized exactly like
# `btrfs-replace-rejected-during-scrub` so a 3000 MiB payload keeps the scrub
# live for ~7-15 seconds at linux-builder's observed ~400 MiB/s rate. LUKS is
# the throttle, not scenery: unencrypted, the same payload scrubs in ~1.5
# seconds, which is not a window the refusals can land in.
{
  name = "repro-btrfs-scrub-start-rejected-during-scrub";

  nodes.machine =
    { pkgs, ... }:
    {
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

      environment.systemPackages = [
        pkgs.cryptsetup
        pkgs.btrfs-progs
      ];
    };

  testScript = builtins.readFile ./btrfs-scrub-start-rejected-during-scrub.py;
}
