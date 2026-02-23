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
# Dependencies: bash braid (init-disk, apply) for pool setup; Rust braid-rust
# binary for status validation.
{ braid-rust }:
{
  name = "braid-status-rust";

  nodes.machine = { pkgs, ... }: let
    braid-cli = pkgs.writeShellApplication {
      name = "braid";
      runtimeInputs = [ pkgs.cryptsetup pkgs.btrfs-progs pkgs.util-linux pkgs.jq pkgs.coreutils ];
      text = builtins.readFile ../scripts/braid.sh;
    };
  in {
    virtualisation.emptyDiskImages = [
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk2"; }
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk3"; }
    ];

    environment.systemPackages = [
      braid-cli
      braid-rust
      pkgs.cryptsetup
      pkgs.btrfs-progs
      pkgs.jq
    ];

    environment.etc."braid/config.json".text = builtins.toJSON {
      disks = [
        "/dev/disk/by-id/virtio-disk1"
        "/dev/disk/by-id/virtio-disk2"
        "/dev/disk/by-id/virtio-disk3"
      ];
      mountPoint = "/mnt/storage";
    };
  };

  testScript = builtins.readFile ./braid-status-rust.py;
}
