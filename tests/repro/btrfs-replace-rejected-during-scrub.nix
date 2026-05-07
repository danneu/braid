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
# Three 4096 MiB disks: disk1 + disk2 form the RAID1 pool, disk3 is the
# standby replacement target. Disks are oversized vs. the existing
# `btrfs-replace-rejects-smaller-target` template so a 3000 MiB payload
# keeps the kernel's `dev->scrub_ctx` live for ~7-15 seconds even at
# linux-builder's unthrottled ~400 MiB/s scrub rate -- a comfortable
# window for the replace ioctl to land while scrub is in progress.
{
  name = "repro-btrfs-replace-rejected-during-scrub";

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
        {
          size = 4096;
          driveConfig.deviceExtraOpts.serial = "disk3";
        }
      ];

      environment.systemPackages = [
        pkgs.cryptsetup
        pkgs.btrfs-progs
      ];
    };

  testScript = builtins.readFile ./btrfs-replace-rejected-during-scrub.py;
}
