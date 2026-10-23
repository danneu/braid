# Test: remove-missing-2disk-collapse
#
# What: `braid remove-missing` against a 2-disk RAID1 pool with one disk
# missing collapses the pool onto the present disk: it converts to single
# data and DUP metadata, then removes the missing devid.
#
# Why: the kernel refuses `btrfs device remove` while RAID1 block groups
# need two devices, so the convert must come first, and it is safe only
# because btrfs keeps a MISSING device off its chunk allocator. Only a real
# kernel proves both
# (docs/design/decisions/012-intent-cli.md#two-to-one-remove).
#
# 2 disks, each 1 GiB. Models the disk-death + degraded-mount sequence of
# `tests/repro/degraded-soft-balance.py`, then drives `braid remove-missing`
# end to end.
{ braid }:
{
  name = "remove-missing-2disk-collapse";

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

  testScript = builtins.readFile ./remove-missing-2disk-collapse.py;
}
