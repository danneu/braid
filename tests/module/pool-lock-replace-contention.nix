# Test: pool-lock-replace-contention
#
# What: Verifies that `braid replace` takes the wrapper pool lock before
# it can read pool state or write pending-op.json.
#
# Why: `replace` has the same preflight-to-journal race shape as
# remove/remove-missing. Without wrapper-level serialization, concurrent
# replace attempts can clobber pending-op.json.tmp before btrfs rejects
# the second kernel replace.
{ braid }:
{ pkgs, lib, ... }:
{
  name = "pool-lock-replace-contention";

  nodes.machine =
    { pkgs, lib, ... }:
    {
      imports = [ ../../modules/braid ];

      braid = {
        enable = true;
        package = braid;
      };

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
        {
          size = 512;
          driveConfig.deviceExtraOpts.serial = "disk4";
        }
      ];
      virtualisation.memorySize = 1024;
    };

  testScript = builtins.readFile ./pool-lock-replace-contention.py;
}
