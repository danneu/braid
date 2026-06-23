# Test: braid add persists pool.json before post-add balance
#
# Intent: verify that `braid add` records the new pool membership after
# `btrfs device add` succeeds and before the post-add RAID1 balance starts.
#
# Why it exists: an interrupted post-add balance used to leave the live btrfs
# pool with N+1 devices while pool.json still listed N devices, making status
# and disk bookkeeping disagree until recovery rebuilt the file.
#
# Scenario: a 1-disk pool with several GiB of single-profile data gains a
# second disk. While the conversion balance is still running, pool.json must
# already contain both disks, enriched metadata for the new disk, and the
# still-present pending operation journal.
{ braid }:
{
  name = "braid-add-persists-before-balance";

  nodes.machine =
    { pkgs, ... }:
    {
      virtualisation.emptyDiskImages = [
        {
          size = 6144;
          driveConfig.deviceExtraOpts.serial = "disk1";
        }
        {
          size = 6144;
          driveConfig.deviceExtraOpts.serial = "disk2";
        }
      ];
      virtualisation.memorySize = 2048;

      environment.systemPackages = [
        braid
        pkgs.cryptsetup
        pkgs.btrfs-progs
      ];

      environment.etc."braid/config.json".text = builtins.toJSON {
        mount_point = "/mnt/storage";
      };
    };

  testScript =
    builtins.readFile ./member_helpers.py
    + "\n\n"
    + builtins.readFile ./braid-add-persists-before-balance.py;
}
