# Test: ups-lb-during-balanced-add
#
# Forces shutdown via upsmon's critical-state SHUTDOWNCMD while the
# post-add `pool_balance_raid1` (the conversion of single-profile
# chunks to RAID1 that runs after `braid add` brings the pool from
# 1 to >=2 devices) is in flight, reboots, and asserts that
# `braid recover` returns the pool to a fully-RAID1 state. The
# recovery path exercises the M1 remediation in `cli/src/recover.rs`
# -- replaying the post-Add soft RAID1 balance to drain any chunks
# left non-RAID1 by the cancelled balance worker.
#
# This is M6 of plans/wip/forced-shutdown-recovery-proof.md and is one
# of the four matrix tests gating the flip of ADR 020 to Active.
#
# 2 disks, 6 GiB each. The test populates a 1-disk pool with ~3 GiB
# of single-profile data, then adds disk2 to trigger the post-add
# balance. The balance has ~3 GiB of conversion work to do on
# tmpfs-backed virtual disks, comfortably wider than the ~1s
# shutdown window from LB detection to umount.
{ braid }:
{ pkgs, lib, ... }:
{
  name = "ups-lb-during-balanced-add";

  nodes.machine =
    { pkgs, lib, ... }:
    {
      imports = [
        ../../modules/braid
        (import ./lib/ups-fixture.nix { })
      ];

      braid = {
        enable = true;
        package = braid;
      };

      virtualisation.emptyDiskImages = [
        { size = 6144; driveConfig.deviceExtraOpts.serial = "disk1"; }
        { size = 6144; driveConfig.deviceExtraOpts.serial = "disk2"; }
      ];
      virtualisation.memorySize = 2048;

      # Persist journal across reboots so the post-reboot subtest can
      # confirm upsmon's SHUTDOWNCMD actually triggered the previous
      # boot's `braid-online.service` ExecStop.
      services.journald.extraConfig = "Storage=persistent";
    };

  testScript = builtins.readFile ./ups-lb-during-balanced-add.py;
}
