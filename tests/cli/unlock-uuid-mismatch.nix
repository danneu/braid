# Test: unlock LUKS UUID mismatch
#
# What: Verifies `braid unlock` fatally errors when a disk's LUKS UUID no longer
# matches the UUID stored in pool.json (swapped, reformatted, or corrupted drive).
#
# Why: Silent wrong-data mount is the highest-blast-radius failure mode. The UUID
# check in `plan_open_pool` is the guard. This test exercises the real
# cryptsetup luksUUID probe → comparison → fatal error pipeline end-to-end.
{ braid }:
{
  name = "unlock-uuid-mismatch";

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

  testScript =
    builtins.readFile ./member_helpers.py
    + "\n\n"
    + builtins.readFile ./unlock-uuid-mismatch.py;
}
