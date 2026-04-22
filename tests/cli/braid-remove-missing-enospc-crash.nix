# Test: braid remove-missing — ENOSPC pre-flight rejection (partial free space)
#
# What: Verifies that `braid remove-missing` rejects when survivors have
# SOME free space but not enough to complete relocation. This is the more
# dangerous scenario — without the check, btrfs starts relocating, partially
# succeeds, then crashes the filesystem to read-only.
#
# Why: The instant-ENOSPC case (zero free space) is caught by
# braid-remove-missing-enospc. This test covers the partial-relocation
# crash scenario from repro-btrfs-remove-enospc-crash.
#
# Dependencies: braid add (builds the test pool).
{ braid }:
{
  name = "braid-remove-missing-enospc-crash";

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

  testScript = builtins.readFile ./braid-remove-missing-enospc-crash.py;
}
