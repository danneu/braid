# Test: remove-missing-2disk-rejected
#
# What: `braid remove-missing` against a 2-disk RAID1 pool with one disk
# missing must reject at preflight, before journaling or acquiring the
# sleep inhibitor, and name the supported repair paths.
#
# Why: The kernel's `btrfs_rm_device` calls
# `btrfs_check_raid_min_devices(num_devices - 1)` and rejects with
# `BTRFS_ERROR_DEV_RAID1_MIN_NOT_MET` whenever the remaining count
# would drop below the RAID1 minimum of 2. Without the preflight,
# braid strands `pending-op.json` and the sleep inhibitor for a
# doomed call, then forces the operator into `braid recover` for an
# operation that was never going to succeed.
#
# 2 disks, each 1 GiB. Models the closest live topology to
# `tests/repro/degraded-soft-balance.py` for the disk-death +
# degraded-mount sequence, then drives `braid remove-missing` end to
# end. A second 2-disk pool is not needed for dry-run coverage --
# the unit test pins the dry-run reject; this VM test only confirms
# the end-to-end real-run + dry-run reject path.
{ braid }:
{
  name = "remove-missing-2disk-rejected";

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

      environment.systemPackages = [
        braid
        pkgs.cryptsetup
        pkgs.btrfs-progs
      ];

      environment.etc."braid/config.json".text = builtins.toJSON {
        mount_point = "/mnt/storage";
      };
    };

  testScript = builtins.readFile ./remove-missing-2disk-rejected.py;
}
