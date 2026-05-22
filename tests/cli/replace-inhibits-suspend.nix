# Test: replace-inhibits-suspend
#
# What: braid replace must hold a logind sleep inhibitor
# (What=sleep, Who=braid, Mode=block) for the duration of the long-running
# btrfs replace operation, releasing it once the replace finishes.
#
# Why: suspending the host mid-replace produces a non-standard 5-device
# topology with a phantom MISSING devid 0 on every kernel (Path A in #48),
# and on v6.19+ kernels also triggers the new freeze/signal cancellation
# path (Path B). Upstream btrfs explicitly recommends inhibiting suspend
# during replace — reference/btrfs-progs/Documentation/btrfs-replace.rst:49-50.
# braid enables autosuspend by default, so this interaction is reachable
# in normal operation.
#
# 4 disks: three pool members (disk1/2/3) and one replacement target (disk4).
# Each is 1024 MiB so the replace has measurable work without inflating runtime.
{ braid }:
{
  name = "replace-inhibits-suspend";

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
        {
          size = 1024;
          driveConfig.deviceExtraOpts.serial = "disk4";
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
    + builtins.readFile ./replace-inhibits-suspend.py;
}
