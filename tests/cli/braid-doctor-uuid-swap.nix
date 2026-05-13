# Test: braid-doctor-uuid-swap
#
# What: `braid doctor` fails when a declared member's live LUKS UUID diverges
# from the UUID key persisted in pool.json.
#
# Why: doctor is the read-only diagnostic surface that should catch a swapped,
# cloned, or reformatted disk before a mutating command reaches the open path.
{ braid }:
{
  name = "braid-doctor-uuid-swap";

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

  testScript = builtins.readFile ./braid-doctor-uuid-swap.py;
}
