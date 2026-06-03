# Test: braid-add-warnings
#
# What: Pins the routing of `braid add`'s planning-phase diagnostics
# across dry-run vs real-run after the PR 7 Preview migration, and the
# degraded-add RAID1-balance skip. Focus is on the missing-devices
# warning plus the `[skip]` balance note -- the keyfile-asymmetry warning
# has its own fixture in `braid-add-enroll.py`, and no-op wording
# preservation is pinned in `braid-add-disk.py`. Phase 3 also empirically
# confirms `btrfs device add` succeeds on a degraded mount and that the
# pool stays degraded (redundancy deferred to `remove-missing`/`replace`).
#
# Why: PR 7 moved `eprintln!("warning: pool has N missing device...")`
# from a raw stderr write into `plan.notes` and dropped the legacy
# `warning:` prefix entirely. Both modes now wrap the same warning body
# via the same `status_line(StatusTag::Warn, ...)` helper and render it
# as `[warn] pool has ...`; only the stream differs -- dry-run to stdout
# (`Preview::render`), real-run to stderr
# (`preview::render_notes_for_stderr`). No legacy `warning:` wording
# survives on either stream; the `.py` asserts `warning: pool has` is
# absent from both the real-run and refusal-path stderr.
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
        # disk4 is the preserved-context fixture: braid-labeled LUKS
        # with no btrfs superblock (ambiguous identity). plan_add
        # accumulates the missing-devices warn from the degraded pool,
        # then add work-plan rendering rejects disk4; stderr must show
        # the canonical `[warn] ...` line BEFORE the refusal error.
        {
          size = 1024;
          driveConfig.deviceExtraOpts.serial = "disk4";
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
