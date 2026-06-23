# Test: config disk-name immutability
#
# What: Verifies mutating braid commands fail fast when an existing by-id path
# is reintroduced under a different disk name.
#
# Why: disk names are presentation/adoption metadata, not identity. A by-id path
# already owned by pool.json must not be silently adopted under a new name.
#
# Dependencies: braid add (to build initial pool membership entries).
{ braid }:
{
  name = "config-name-immutability";

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
        pkgs.jq
      ];

      environment.etc."braid/config.json".text = builtins.toJSON {
        mount_point = "/mnt/storage";
      };
    };

  testScript =
    builtins.readFile ./member_helpers.py
    + "\n\n"
    + builtins.readFile ./config-name-immutability.py;
}
