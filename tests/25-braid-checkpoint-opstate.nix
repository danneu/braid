# Test: checkpoint op-state strict resumability
#
# What: Exercises strict checkpoint/resume behavior for intent CLI by validating
# deterministic interruption, strict config-drift rejection, and bounded phase
# pause timeout handling.
#
# Why: Checkpoint correctness is safety-critical. Resume must be deterministic,
# fail-closed on drift/corruption, and never hang indefinitely in CI.
#
# Dependencies: braid add lifecycle and btrfs RAID1 conversion behavior.
{ braid }:
{
  name = "braid-checkpoint-opstate";

  nodes.machine = { pkgs, ... }: {
    virtualisation.emptyDiskImages = [
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk2"; }
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk3"; }
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk4"; }
    ];

    environment.systemPackages = [
      braid
      pkgs.cryptsetup
      pkgs.btrfs-progs
      pkgs.jq
      pkgs.coreutils
    ];
  };

  testScript = builtins.readFile ./braid-checkpoint-opstate.py;
}
