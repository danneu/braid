# Test: braid doctor offline declared member.
#
# Intent: boots a real two-disk braid pool so doctor can compare LUKS identity
#   against mounted btrfs membership.
# Why it exists: unit tests cannot make `std::fs::metadata` report a real block
#   device for declared_disks.
# Scenario: one declared member is present on disk but absent from a degraded
#   live btrfs mount.
{ braid }:
{
  name = "braid-doctor-offline-member";

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

  testScript = builtins.readFile ./braid-doctor-offline-member.py;
}
