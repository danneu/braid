# Test: remove-missing-membership-readonly
#
# What: `braid remove-missing` must fail hard (not warn) when pool.json
# cannot be written.
#
# Why: remove_missing.rs:158-161 only warns on save_membership failure and
# proceeds with btrfs device deletion. This lets btrfs state diverge from
# pool.json — the missing device is removed from btrfs but pool.json still
# claims it exists.
#
# Dependencies: braid add (builds the test pool).
{ braid }:
{
  name = "remove-missing-membership-readonly";

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

  testScript = builtins.readFile ./remove-missing-membership-readonly.py;
}
