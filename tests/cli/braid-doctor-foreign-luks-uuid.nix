# Test: braid-doctor-foreign-luks-uuid
#
# What: `braid doctor` fails when the live btrfs pool contains a LUKS UUID
# that is absent from pool.json membership.
#
# Why: status redirects operators to doctor for foreign live mappers; doctor
# must provide the persistent structured diagnosis.
{ braid }:
{
  name = "braid-doctor-foreign-luks-uuid";

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
        {
          size = 1024;
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

  testScript = builtins.readFile ./braid-doctor-foreign-luks-uuid.py;
}
