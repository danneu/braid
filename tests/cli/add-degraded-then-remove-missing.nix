# Test: add-degraded-then-remove-missing
#
# What: Pins the safety rationale for the degraded-add RAID1-balance skip.
# `braid add` into a degraded pool joins the disk but skips the hard RAID1
# convert (pool stays degraded, new disk stays empty); the subsequent
# `braid remove-missing` is what actually restores redundancy across the two
# present devices. End-to-end on real btrfs + LUKS.
#
# Why: the degraded add deliberately defers redundancy restoration to the
# purpose-built repair path. This is the documented `add`-then-`remove-missing`
# workflow for a 2-disk degraded pool (`remove-missing` alone refuses below two
# devices). Only unit tests pin the skip gate; this proves the full operator
# recipe converges to a healthy RAID1 pool.
#
# Scenario: a 2-disk RAID1 NAS loses disk2. The operator mounts degraded, runs
# `braid add disk3` (the pool stays degraded, disk3 empty), then
# `braid remove-missing --missing-id <devid>` to drop the dead member and
# rebalance onto disk3 -- ending healthy with disk1 + disk3.
{ braid }:
{
  name = "add-degraded-then-remove-missing";

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
        braid
        pkgs.cryptsetup
        pkgs.btrfs-progs
      ];

      environment.etc."braid/config.json".text = builtins.toJSON {
        mount_point = "/mnt/storage";
      };
    };

  testScript = builtins.readFile ./add-degraded-then-remove-missing.py;
}
