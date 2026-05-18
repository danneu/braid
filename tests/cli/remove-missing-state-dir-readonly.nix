# Test: remove-missing-state-dir-readonly
#
# What: `braid remove-missing` must fail hard when the pending-operation
# journal cannot be written. The btrfs pool must stay intact.
#
# Why: Per ADR-017 ("Mutation ordering"), pending-op.json is written
# before the irreversible btrfs membership change. If that write fails
# (read-only state dir, ENOSPC, permissions), remove-missing must abort
# before any btrfs mutation -- otherwise btrfs and pool.json could
# diverge with no journal to drive recovery.
#
# Dependencies: braid add (builds the test pool).
{ braid }:
{
  name = "remove-missing-state-dir-readonly";

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

  testScript = builtins.readFile ./remove-missing-state-dir-readonly.py;
}
