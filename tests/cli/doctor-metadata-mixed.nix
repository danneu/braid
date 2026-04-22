# Test: braid doctor detects mixed metadata profiles
#
# What: Creates a btrfs RAID1 pool with mixed metadata profiles
# (some RAID1, some single) and verifies braid doctor warns.
#
# Why: Mixed metadata is more dangerous than mixed data — metadata loss
# can make the entire filesystem unrecoverable. This test proves the
# metadata_profile_mismatch check works against a real btrfs filesystem.
#
# How: Fills the initial 256 MiB metadata block group with inline files
# (< 2048 bytes each, stored in the metadata B-tree). Once full, btrfs
# allocates a second metadata BG. Then limit=1 converts exactly one BG
# to single, creating a RAID1 + single mix.
#
# Dependencies: braid add, braid doctor.
{ braid }:
{
  name = "doctor-metadata-mixed";

  nodes.machine =
    { pkgs, ... }:
    {
      virtualisation.emptyDiskImages = [
        {
          size = 2048;
          driveConfig.deviceExtraOpts.serial = "disk1";
        }
        {
          size = 2048;
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

  testScript = builtins.readFile ./doctor-metadata-mixed.py;
}
