# Test: ups-lb-during-remove
#
# Forces shutdown via upsmon's critical-state SHUTDOWNCMD while a
# `braid remove disk3` is in flight, reboots, and asserts that
# `braid recover` returns the pool to a clean state. The remove either
# rolls back to the pre-membership (3 devices, operator re-runs braid
# remove to finish) or completes (2 devices) -- both are legitimate
# recovered states.
#
# This is M4 of plans/wip/forced-shutdown-recovery-proof.md and is one
# of the four matrix tests gating the flip of ADR 020 to Active.
#
# 3 disks, 6 GiB each. The 5 GiB urandom payload makes the remove's
# data relocation last several seconds on tmpfs-backed virtual disks --
# wider than the ~1s shutdown window from LB detection to umount --
# so the test reliably catches the interrupted-remove scenario.
{ braid }:
{ pkgs, lib, ... }:
{
  name = "ups-lb-during-remove";

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

      # Disk sizing has two competing pressures:
      #
      #   - The ENOSPC preflight in `cli/src/preflight.rs:check_raid1_relocation_space`
      #     requires the surviving 2 disks to absorb the 3rd disk's allocated
      #     chunks. With a 3-disk RAID1, each disk holds roughly 2/3 of the
      #     payload's raw bytes; the survivors need ~3x the disk-being-removed's
      #     allocated chunks in unallocated headroom (RAID1 capacity = total/2).
      #     For a 3 GiB payload, each disk allocates ~2 GiB of chunks; the
      #     survivors need ~2 GiB unalloc each; so disks must be at least
      #     ~6 GiB. We use 10 GiB to give comfortable headroom past that
      #     boundary so the preflight does not become flaky on chunk-size
      #     rounding.
      #
      #   - The remove must stay in flight longer than the ~1s shutdown
      #     window from LB detection to umount. Tmpfs-backed virtual disks
      #     relocate at ~1 GiB/s, so a 3 GiB payload (= ~2 GiB to relocate
      #     when removing one of three disks) keeps the remove in flight
      #     for ~2s.
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

  testScript = builtins.readFile ./ups-lb-during-remove.py;
}
