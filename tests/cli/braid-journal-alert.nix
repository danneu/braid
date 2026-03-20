# Test: kernel journal alert lifecycle
#
# What: Validates the full alert lifecycle for kernel-journal-detected btrfs
# errors: dm-flakey injection → journal scan → monitor exits 1 → status shows
# kernel storage error → re-monitor (latch persists) → ack → re-monitor (clear).
#
# Why: Journal entries are events, not pollable state. Once the cursor advances
# past them, they can't be re-detected. This test proves that journal causes
# are durably merged into the latch and persist across monitor cycles until ack.
#
# Scenario: 2-disk RAID1 pool. dm-flakey injects write errors on one disk.
# A write failure produces kernel journal "BTRFS error" entries. `braid monitor`
# detects them and exits 1. `braid status` shows the banner with disk name.
# A second `braid monitor` still exits 1 (latch persists even though cursor
# advanced). `braid ack` clears everything. Final `braid monitor` exits 0.
{ braid }:
{
  name = "braid-journal-alert";

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
      pkgs.kmod
      pkgs.lvm2
    ];

    environment.etc."braid/config.json".text = builtins.toJSON {
      disks = {
        disk1 = { by_id = "/dev/disk/by-id/virtio-disk1"; };
        disk2 = { by_id = "/dev/disk/by-id/virtio-disk2"; };
      };
      mount_point = "/mnt/storage";
    };
  };

  testScript = builtins.readFile ./braid-journal-alert.py;
}
