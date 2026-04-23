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
# 3 disks, 10 GiB each. The test populates the pool, kills disk2 by
# closing its LUKS mapper, remounts degraded, writes additional data
# to create single-profile chunks, then runs `braid remove-missing`.
# The fast metadata-only `btrfs device delete missing` runs first;
# the soft balance is the long phase the test interrupts.
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

      # 10 GiB disks. Phase 2 writes a 512 MiB RAID1 baseline; Phase 3
      # writes a 3 GiB single-profile payload to disk1 while disk2 is
      # down. Btrfs over-allocates chunks during single-profile writes,
      # so disk1 ends the degraded phase with ~5.4 GiB of chunks
      # allocated. `braid remove-missing` preflight
      # (`check_raid1_relocation_space` in cli/src/preflight.rs)
      # requires `raid1_capacity >= Data allocated on the missing
      # device`, observed at ~2 GiB. With 6 GiB disks, disk1
      # unallocated drops to ~696 MiB and preflight refuses before the
      # soft balance starts; 10 GiB leaves ~4.7 GiB unallocated, ~2.3x
      # the preflight threshold -- headroom for btrfs chunk-allocator
      # variance across nixpkgs bumps. Disk3 (the replacement) is
      # sized the same so the post-recovery pool is symmetric.
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
    };

  testScript = builtins.readFile ./ups-lb-during-remove-missing.py;
}
