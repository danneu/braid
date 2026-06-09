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
# This is M6 of plans/impl/2026-04-21-forced-shutdown-recovery-proof.md and is one
# of the four matrix tests gating the flip of ADR 020 to Active.
#
# 2 disks, 6 GiB each. The test populates a 1-disk pool with a small
# single-profile payload, then adds disk2 through dm-delay to trigger an
# observable post-add balance during the UPS shutdown window.
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
        {
          size = 6144;
          driveConfig.deviceExtraOpts.serial = "disk1";
        }
        {
          size = 6144;
          driveConfig.deviceExtraOpts.serial = "disk2";
        }
      ];
      virtualisation.memorySize = 2048;

      # Persist journal across reboots so the post-reboot subtest can
      # confirm upsmon's SHUTDOWNCMD actually triggered the previous
      # boot's `braid-online.service` ExecStop.
      services.journald.extraConfig = "Storage=persistent";

      environment.systemPackages = [
        pkgs.lvm2
      ];
    };

  testScript =
    builtins.readFile ./dm_delay_helpers.py + "\n\n" + builtins.readFile ./ups-lb-during-balanced-add.py;
}
