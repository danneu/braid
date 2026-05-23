# Test: remove-inhibits-suspend
#
# What: braid remove must hold a logind sleep inhibitor
# (What=sleep, Who=braid, Mode=block) for the duration of its mutation
# window, releasing it once remove finishes.
#
# Why: suspending the host mid-remove can leave the kernel-side
# device-remove state machine in a partially-relocated state requiring
# recovery, and braid enables autosuspend by default, so this interaction
# is reachable in normal operation. See docs/design/decisions/019-inhibit-sleep.md
# for the boundary rule.
#
# Topology choice — 2 disks, not 3:
#   This test deliberately exercises the 2→1 removal path. In a 2-disk
#   RAID1 pool, removing one disk leaves only 1 device, which makes
#   RemovePlan::execute in cli/src/remove.rs run pool_balance_single()
#   *before* btrfs device remove. That pre-balance is the long, reliably
#   observable phase the inhibitor must protect.
#
#   The 3→2 path was tried first and removed: in that topology,
#   pool_balance_single is skipped (it only runs when `remaining == 1`),
#   leaving only the kernel `btrfs device remove` step, which on the
#   test runner's fast virtual disks completes well under a second —
#   before any external poll can catch it. The 2→1 path is a real
#   cmd_remove mutation window with the same inhibitor seam, but with
#   a long enough phase to make the VM test stable.
#
# 2 x 2048 MiB disks. The test now uses a small payload and dm-delay on
# both surviving candidates so the RAID1->single rebalance is observable
# without filling the filesystem with timing-only data.
{ braid }:
{
  name = "remove-inhibits-suspend";

  nodes.machine =
    { pkgs, ... }:
    {
      virtualisation.emptyDiskImages = [
        {
          size = 2048;
          driveConfig.deviceExtraOpts.serial = "disk1";
        }
        {
          size = 2048;
          driveConfig.deviceExtraOpts.serial = "disk2";
        }
      ];

      environment.systemPackages = [
        braid
        pkgs.cryptsetup
        pkgs.btrfs-progs
        pkgs.lvm2
      ];

      environment.etc."braid/config.json".text = builtins.toJSON {
        mount_point = "/mnt/storage";
      };
    };

  # Inhibitor query helpers (list_inhibitors / find_braid_sleep_inhibitor)
  # are concatenated into the test script's global namespace at Nix-eval
  # time. NixOS VM tests run testScript as a single Python string with no
  # module path, so a normal `import` would not work — see
  # tests/cli/inhibitor_helpers.py for details.
  testScript =
    builtins.readFile ./../module/dm_delay_helpers.py + "\n\n"
    + builtins.readFile ./inhibitor_helpers.py + "\n\n"
    + builtins.readFile ./remove-inhibits-suspend.py;
}
