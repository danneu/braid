# Test: braid bootstrap (first-disk via plan/apply)
#
# What: Exercises the full bootstrap workflow — creating a pool from scratch using
# `braid plan` and `braid apply` instead of `braid-add-disk`.
#
# Why: The unified CLI must handle day-one setup identically to steady-state changes.
# No pool mounted → plan detects all configured disks as additions → apply creates the
# pool. This validates that the planner's bootstrap path works end-to-end.
#
# Dependencies: braid plan/apply bootstrap path (no pool required).
{ braid }:
{
  name = "braid-bootstrap";

  nodes.machine = { pkgs, ... }: {
    virtualisation.emptyDiskImages = [
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk2"; }
    ];

    environment.systemPackages = [
      braid
      pkgs.cryptsetup
      pkgs.btrfs-progs
      pkgs.jq
    ];
  };

  testScript = builtins.readFile ./braid-bootstrap.py;
}
