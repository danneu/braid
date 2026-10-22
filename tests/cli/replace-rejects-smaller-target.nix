# Test: replace-rejects-smaller-target
#
# What: `braid replace` refuses an undersized replacement before journal write
# or LUKS formatting, for both live and missing source devices. It accepts an
# equal-size fresh target whose optimal I/O size would make cryptsetup's own
# layout start the data later, because braid pins the LUKS data offset the
# preflight models.
#
# Why: btrfs rejects smaller replace targets only after braid used to format
# the target and write `pending-op.json`; braid now preflights the same size
# comparison before destructive work, and the fresh-target model holds only
# while luksFormat uses the offset the preflight subtracts.
#
# Dependencies: braid add (builds the test pool), braid replace live and
# missing planning paths, real kernel `BTRFS_IOC_DEV_INFO`, QEMU virtio-blk
# topology hints (`opt_io_size`, `min_io_size`).
{ braid }:
{
  name = "replace-rejects-smaller-target";

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
        {
          size = 256;
          driveConfig.deviceExtraOpts.serial = "disk3";
        }
        {
          # Same size as disk2, but reports a 32 MiB optimal I/O size. Left
          # to itself, cryptsetup would round the LUKS data offset up to
          # 32 MiB and leave a mapper 16 MiB smaller than disk2's.
          size = 512;
          driveConfig.deviceExtraOpts = {
            serial = "disk4";
            min_io_size = "4096";
            opt_io_size = "33554432";
          };
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

  testScript = builtins.readFile ./replace-rejects-smaller-target.py;
}
