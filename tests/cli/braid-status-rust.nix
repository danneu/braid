# Test: braid status (Rust implementation)
#
# What: Validates the Rust braid status subcommand end-to-end against real
# virtual disks — single-disk, RAID1, degraded, and not-mounted states in both
# human and JSON output modes.
#
# Why: The Rust CLI must produce correct status reports from real disk state.
# This test bridges unit tests (pure logic) with integration: real LUKS, real
# btrfs, real command output parsed by the Rust probe and status layers.
#
# Dependencies: Rust braid binary for all commands.
{ braid }:
{
  name = "braid-status-rust";

  nodes.machine = { pkgs, ... }: {
    virtualisation.emptyDiskImages = [
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk2"; }
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk3"; }
    ];

    environment.systemPackages = [
      braid
      pkgs.cryptsetup
      pkgs.btrfs-progs
      pkgs.jq
    ];

    environment.etc."braid/config.json".text = builtins.toJSON {
      disks = {
        disk1 = { by_id = "/dev/disk/by-id/virtio-disk1"; };
        disk2 = { by_id = "/dev/disk/by-id/virtio-disk2"; };
        disk3 = { by_id = "/dev/disk/by-id/virtio-disk3"; };
      };
      mount_point = "/mnt/storage";
    };
  };

  testScript = builtins.readFile ./braid-status-rust.py;
}
