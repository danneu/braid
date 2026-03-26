# Test: smartd alert lifecycle
#
# What: Validates smartd-triggered alerts appear in `braid status` and clear
# with `braid ack`.
#
# Why: smartd alerts bridge external SMART monitoring into braid's alert
# model via a flag file. This test verifies the flag file → alert →
# ack → clear lifecycle without requiring actual SMART hardware.
#
# Scenario: Healthy 2-disk RAID1 pool. smartd detects a SMART attribute
# change and touches the alert flag file. `braid monitor` exits 1.
# `braid status` shows SMART warning. `braid ack` removes the flag.
{ braid }:
{
  name = "braid-smartd-alert";

  nodes.machine = { pkgs, ... }: {
    virtualisation.emptyDiskImages = [
      { size = 512; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 512; driveConfig.deviceExtraOpts.serial = "disk2"; }
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

  testScript = builtins.readFile ./braid-smartd-alert.py;
}
