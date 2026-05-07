# Test: recover-replace-existing-luks-uuid-mismatch
#
# What: `braid recover` after a crash mid-`replace --enroll DIR`,
# but with the new disk SWAPPED before recovery runs (different LUKS
# UUID than the journaled value), must refuse cleanly. The journal
# is preserved (NOT cleared), the operator gets the canonical
# "preserving pending-op.json" remediation hint, and no LUKS
# mutation runs.
#
# Why: defensive identity probe in the ExistingLuks recovery arm.
# Pre-refactor, recovery had no probe and would silently roll back
# even if the user replugged a different disk. Pinning that the
# journal survives + no mutation runs prevents a regression that
# would let recovery proceed on the wrong disk.
{ braid }:
{
  name = "recover-replace-existing-luks-uuid-mismatch";

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
        {
          size = 1024;
          driveConfig.deviceExtraOpts.serial = "disk4";
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

  testScript = builtins.readFile ./recover-replace-existing-luks-uuid-mismatch.py;
}
