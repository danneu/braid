# Test: braid status during balance
#
# What: Validates that braid status succeeds while a RAID1 balance is in
# progress (paused), when the data ratio is a fractional value like "1.01".
#
# Why: During a single→RAID1 balance, btrfs reports intermediate data ratios.
# The parser previously only accepted "1.00" and "2.00", causing braid status
# to hard-error during any balance operation.
#
# How: Starts a balance and immediately pauses it to guarantee a stable
# mid-balance window, avoiding the race where the balance completes before
# the test can observe it.
#
# Dependencies: Rust braid binary for all commands.
{ braid }:
{
  name = "braid-status-during-balance";

  nodes.machine =
    { pkgs, ... }:
    {
      virtualisation.emptyDiskImages = [
        {
          size = 4096;
          driveConfig.deviceExtraOpts.serial = "disk1";
        }
        {
          size = 4096;
          driveConfig.deviceExtraOpts.serial = "disk2";
        }
      ];

      virtualisation.memorySize = 2048;

      environment.systemPackages = [
        braid
        pkgs.cryptsetup
        pkgs.btrfs-progs
        pkgs.lvm2
        pkgs.jq
      ];

      environment.etc."braid/config.json".text = builtins.toJSON {
        mount_point = "/mnt/storage";
      };
    };

  testScript =
    builtins.readFile ./../module/dm_delay_helpers.py
    + "\n\n"
    + builtins.readFile ./../module/balance_helpers.py
    + "\n\n"
    + builtins.readFile ./braid-status-during-balance.py;
}
