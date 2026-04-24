# Test: braid-add-warnings
#
# What: Pins the routing of `braid add`'s planning-phase diagnostics
# across dry-run vs real-run after the PR 7 Preview migration. Focus is
# on the missing-devices warning -- the keyfile-asymmetry warning has
# its own fixture in `braid-add-enroll.py`, and no-op wording
# preservation is pinned in `braid-add-disk.py`.
#
# Why: PR 7 moved `eprintln!("warning: pool has N missing device...")`
# from a raw stderr write into `plan.notes`. Dry-run must emit the
# canonical `[warn]  pool has ...` body-only form on stdout; real-run
# must preserve today's `warning: pool has ...` stderr wording
# byte-identically so log scrapers do not drift.
#
# Scenario: operator builds a 2-disk RAID1 pool, one drive dies (mapper
# closed, pool remounted -o degraded), operator tries to add a
# replacement disk via `braid add` instead of `braid replace`.
{ braid }:
{
  name = "braid-add-warnings";

  nodes.machine =
    { pkgs, ... }:
    {
      virtualisation.emptyDiskImages = [
        {
          size = 1024;
          driveConfig.deviceExtraOpts.serial = "disk1";
        }
        {
          size = 1024;
          driveConfig.deviceExtraOpts.serial = "disk2";
        }
        {
          size = 1024;
          driveConfig.deviceExtraOpts.serial = "disk3";
        }
      ];

      environment.systemPackages = [
        braid
        pkgs.cryptsetup
        pkgs.btrfs-progs
      ];

      environment.etc."braid/config.json".text = builtins.toJSON {
        mount_point = "/mnt/storage";
      };
    };

  testScript = builtins.readFile ./braid-add-warnings.py;
}
