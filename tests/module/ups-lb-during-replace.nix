# Test: ups-lb-during-replace
#
# Forces shutdown via upsmon's critical-state SHUTDOWNCMD while a
# `braid replace` is in flight, reboots, and asserts that `braid recover`
# returns the pool to a clean state with the replace either completed or
# cleanly resumable.
#
# This is M3 of plans/wip/forced-shutdown-recovery-proof.md and is one
# of the four matrix tests that gate flipping ADR 020 to Active.
#
# 4 disks: disk1/2/3 are pool members (1024 MiB each), disk4 is the
# replacement target. The 400 MiB urandom payload that disk2 holds
# matches the staging in tests/repro/btrfs-replace-interrupted-mid-flight.py
# so the replace runs long enough to reliably catch the in-flight state
# from the test script.
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

      # Disks must be large enough that the replace stays in-flight
      # longer than the shutdown sequence between LB detection and
      # umount. On tmpfs-backed virtual disks the replace runs at ~1
      # GiB/s, while the shutdown window is ~1s after FINALDELAY hits
      # 0 (lib/ups-fixture.nix). 4 GiB disks with a 3 GiB urandom
      # payload give ~3s of replace work, comfortably wider than the
      # shutdown window.
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
    };

  testScript = builtins.readFile ./ups-lb-during-replace.py;
}
