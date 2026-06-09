# Test: ups-lb-during-remove-missing
#
# Forces shutdown via upsmon's critical-state SHUTDOWNCMD while the
# `maybe_restore_raid1` soft balance triggered by `braid remove-missing`
# is in flight, reboots, and asserts that `braid recover` returns the
# pool to a clean RAID1 state. The recovery path exercises the M1
# remediation in `cli/src/recover.rs` -- detecting the paused soft
# balance and resuming it before clearing the journal -- so without
# the M1 fix this test would leave the pool with single-profile
# chunks unprotected by RAID1 redundancy.
#
# This is M5 of plans/impl/2026-04-21-forced-shutdown-recovery-proof.md and is one
# of the four matrix tests gating the flip of ADR 020 to Active.
#
# 3 disks, 10 GiB each. The test populates the pool, kills disk2 by
# closing its LUKS mapper, remounts degraded, writes a small payload to
# create single-profile chunks, then runs `braid remove-missing` with
# dm-delay on the remaining disks. The fast metadata-only
# `btrfs device delete missing` runs first; the soft balance is the
# long phase the test interrupts.
{ braid }:
{ pkgs, lib, ... }:
{
  name = "ups-lb-during-remove-missing";

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

      # Keep the historical disk size so this test continues to exercise
      # the same degraded/remove-missing capacity shape. dm-delay, not
      # payload size, controls the in-flight timing window.
      virtualisation.emptyDiskImages = [
        {
          size = 10240;
          driveConfig.deviceExtraOpts.serial = "disk1";
        }
        {
          size = 10240;
          driveConfig.deviceExtraOpts.serial = "disk2";
        }
        {
          size = 10240;
          driveConfig.deviceExtraOpts.serial = "disk3";
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
    builtins.readFile ./dm_delay_helpers.py + "\n\n" + builtins.readFile ./ups-lb-during-remove-missing.py;
}
