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
# This is M5 of plans/wip/forced-shutdown-recovery-proof.md and is one
# of the four matrix tests gating the flip of ADR 020 to Active.
#
# 3 disks, 8 GiB each. The test populates the pool, kills disk3 by
# closing its LUKS mapper, remounts degraded, writes additional data
# to create single-profile chunks, then runs `braid remove-missing`.
# The fast metadata-only `btrfs device delete missing` runs first; the
# soft balance is the long phase the test interrupts.
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

      # 6 GiB disks comfortably hold the baseline RAID1 (~512 MiB)
      # plus the 1.5 GiB degraded-write payload that becomes the soft
      # balance's input. Disk3 (the replacement) is sized the same so
      # the pool is balanced after recovery.
      virtualisation.emptyDiskImages = [
        { size = 6144; driveConfig.deviceExtraOpts.serial = "disk1"; }
        { size = 6144; driveConfig.deviceExtraOpts.serial = "disk2"; }
        { size = 6144; driveConfig.deviceExtraOpts.serial = "disk3"; }
      ];
      virtualisation.memorySize = 2048;

      # Persist journal across reboots so the post-reboot subtest can
      # confirm upsmon's SHUTDOWNCMD actually triggered the previous
      # boot's `braid-online.service` ExecStop.
      services.journald.extraConfig = "Storage=persistent";
    };

  testScript = builtins.readFile ./ups-lb-during-remove-missing.py;
}
