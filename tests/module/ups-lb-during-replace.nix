# Test: ups-lb-during-replace
#
# Forces shutdown via upsmon's critical-state SHUTDOWNCMD while a
# `braid replace` is in flight, reboots, and asserts that `braid recover`
# returns the pool to a clean state with the replace either completed or
# cleanly resumable.
#
# This is M3 of plans/impl/2026-04-21-forced-shutdown-recovery-proof.md and is one
# of the four matrix tests that gate flipping ADR 020 to Active.
#
# 4 disks: disk1/2/3 are pool members, disk4 is the replacement target.
# A small payload plus dm-delay on disk4 makes the replace stay in flight
# long enough to reliably trigger the UPS shutdown window.
{ braid }:
{ pkgs, lib, ... }:
{
  name = "ups-lb-during-replace";

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

      # Disk size remains generous enough for replace/recover capacity
      # behavior, while dm-delay controls the in-flight timing window.
      virtualisation.emptyDiskImages = [
        {
          size = 4096;
          driveConfig.deviceExtraOpts.serial = "disk1";
        }
        {
          size = 4096;
          driveConfig.deviceExtraOpts.serial = "disk2";
        }
        {
          size = 4096;
          driveConfig.deviceExtraOpts.serial = "disk3";
        }
        {
          size = 4096;
          driveConfig.deviceExtraOpts.serial = "disk4";
        }
      ];
      virtualisation.memorySize = 2048;

      # Persist journal across reboots so the post-reboot subtest can
      # confirm upsmon's SHUTDOWNCMD actually triggered the previous
      # boot's `braid-online.service` ExecStop (decision 018).
      services.journald.extraConfig = "Storage=persistent";

      environment.systemPackages = [
        pkgs.lvm2
      ];
    };

  testScript =
    builtins.readFile ./dm_delay_helpers.py + "\n\n" + builtins.readFile ./ups-lb-during-replace.py;
}
