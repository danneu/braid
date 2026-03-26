# Test: braid monitor + ack lifecycle
#
# What: Validates the full alert lifecycle for btrfs-detected issues:
# detection via `braid monitor` → banner in `braid status` → acknowledgment
# via `braid ack` → alert cleared.
#
# Why: The alert system must correctly detect missing devices, surface them
# in both the exit code of `braid monitor` and the banner in `braid status`,
# and clear them when the user runs `braid ack`.
#
# Scenario: 3-disk RAID1 pool. Operator loses a disk (LUKS mapper closed,
# simulating a failed drive). `braid monitor` detects the missing device
# and exits 1. `braid status` shows the ALERT banner. `braid ack`
# acknowledges the alert, silencing it. Subsequent `braid monitor` exits 0.
{ braid }:
{
  name = "braid-monitor";

  nodes.machine = { pkgs, ... }: {
    virtualisation.emptyDiskImages = [
      { size = 512; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 512; driveConfig.deviceExtraOpts.serial = "disk2"; }
      { size = 512; driveConfig.deviceExtraOpts.serial = "disk3"; }
    ];

    environment.systemPackages = [
      braid
      pkgs.cryptsetup
      pkgs.btrfs-progs
      pkgs.jq
    ];

    environment.etc."braid/config.json".text = builtins.toJSON {
      mount_point = "/mnt/storage";
    };
  };

  testScript = builtins.readFile ./braid-monitor.py;
}
