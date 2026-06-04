# Test: ack cleanup-pending cross-command contract
#
# What: Validates the produce -> surface -> consume cycle for the
# alert-cleanup-pending sentinel across the real `braid ack` and
# `braid status` binaries over the production /var/lib/braid path.
#
# Why: when ack reaches cleanup and a later step fails, it leaves a
# cleanup-pending sentinel; status must surface it and a sentinel-only
# ack must re-enter cleanup. The unit suite covers both halves with an
# injected runner on a temp dir, but no test drives the real binaries
# against /var/lib/braid -- a wiring regression would pass every unit test.
#
# Scenario: Healthy 2-disk RAID1 pool. A forced cleanup failure (a
# directory poisoning alert-latch.json.corrupt) makes a mounted ack exit
# 1 after marking the sentinel. `braid status` shows the cleanup-pending
# cause. The operator fixes the fault and re-runs `braid ack`, which
# clears the sentinel.
{ braid }:
{
  name = "braid-ack-cleanup-pending";

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
      ];

      environment.systemPackages = [
        braid
        pkgs.cryptsetup
        pkgs.btrfs-progs
        pkgs.jq
      ];

      environment.etc."braid/config.json".text = builtins.toJSON {
        mount_point = "/mnt/storage";
      };
    };

  testScript = builtins.readFile ./braid-ack-cleanup-pending.py;
}
