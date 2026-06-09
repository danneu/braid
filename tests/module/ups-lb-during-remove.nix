# Test: ups-lb-during-remove
#
# Forces shutdown via upsmon's critical-state SHUTDOWNCMD while a
# `braid remove disk3` is in flight, reboots, and asserts that
# `braid recover` returns the pool to a clean state. The remove either
# rolls back to the pre-membership (3 devices, operator re-runs braid
# remove to finish) or completes (2 devices) -- both are legitimate
# recovered states.
#
# This is M4 of plans/impl/2026-04-21-forced-shutdown-recovery-proof.md and is one
# of the four matrix tests gating the flip of ADR 020 to Active.
#
# 3 disks, 10 GiB each. The payload is small; dm-delay on the remove
# source and remaining disks makes relocation last long enough to
# reliably catch the UPS interrupted-remove scenario.
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

      # Keep the historical disk size so this test continues to exercise
      # the same remove/recover capacity shape. dm-delay, not payload size,
      # controls the in-flight timing window.
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
    builtins.readFile ./dm_delay_helpers.py + "\n\n" + builtins.readFile ./ups-lb-during-remove.py;
}
