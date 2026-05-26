# Test: braid remove-missing -- ENOSPC pre-flight fails closed
#
# What: Verifies that `braid remove-missing` refuses when the ENOSPC
# relocation-space pre-flight cannot run `btrfs device usage --raw`.
# Both dry-run and real-run must fail before mutation.
#
# Why: remove-missing runs against a degraded pool. A failed relocation
# safety probe cannot be treated as best-effort without risking a
# read-only filesystem crash and a stranded pending-op.json.
#
# The test forces the fail-closed branch by wrapping `btrfs` on PATH
# with a shim that fails the single `btrfs device usage --raw` call
# issued by `check_relocation_space`.
#
# Dependencies: braid add (builds the test pool).
{ braid }:
{
  name = "braid-remove-missing-preflight-fails-closed";

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
        {
          size = 512;
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

  testScript = builtins.readFile ./braid-remove-missing-preflight-fails-closed.py;
}
