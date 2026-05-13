# Test: luks-mapper-drift
#
# What: Verifies that `braid lock` closes the observed member-owned mapper
# name when a LUKS device is open under a drifted mapper.
#
# Why: Mapper names are runtime handles, not identity. If lock reconstructs
# `braid-<name>` from pool.json instead of using the live UUID-classified
# mapper, a drifted mapper can remain open after lock.
#
# Scenario: A two-disk pool is locked, then disk1 is manually reopened as
# `braid-WRONG` while disk2 uses the normal mapper. The pool is mounted from
# the drifted mapper. `braid lock` must close `braid-WRONG`, and a later
# `braid unlock` must restore the normal mapper names.
{ braid }:
{
  name = "luks-mapper-drift";

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
        pkgs.btrfs-progs
        pkgs.cryptsetup
      ];

      environment.etc."braid/config.json".text = builtins.toJSON {
        mount_point = "/mnt/storage";
      };
    };

  testScript = builtins.readFile ./luks-mapper-drift.py;
}
