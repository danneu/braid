# Test: status-mapper-drift
#
# What: Verifies that `braid status` resolves a drifted live mapper through
# the member LUKS UUID rather than deriving the displayed name from the mapper.
#
# Why: Mapper names are runtime handles, not identity. If status reconstructs
# names from the live mapper instead of using UUID-keyed pool membership, the
# user-visible surface can contradict the disk identity model.
#
# Scenario: A two-disk pool is locked, then disk1 is manually reopened as
# `braid-WRONG` while disk2 uses the normal mapper. The pool is mounted from
# the drifted mapper. `braid status` must show disk1 as the operator-facing
# name while still reporting the observed drifted mapper.
{ braid }:
{
  name = "status-mapper-drift";

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

  testScript = builtins.readFile ./status-mapper-drift.py;
}
