# Test: braid remove-missing — ENOSPC pre-flight rejection
#
# What: Verifies that `braid remove-missing` rejects the operation when
# surviving devices lack space to absorb the missing device's data.
#
# Why: Without this check, btrfs either fails with ENOSPC (harmless) or
# starts relocating, hits ENOSPC mid-transaction, and crashes the
# filesystem to read-only. The pre-flight check prevents both.
#
# Dependencies: braid add (builds the test pool).
{ braid }:
{
  name = "braid-remove-missing-enospc";

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
          size = 512;
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

  testScript = builtins.readFile ./braid-remove-missing-enospc.py;
}
