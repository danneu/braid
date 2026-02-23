# Test: braid plan (Rust implementation)
#
# What: Validates the Rust braid plan subcommand end-to-end against real virtual
# disks — probe layer, planner logic, JSON/human output — matching the bash
# braid-plan test coverage.
#
# Why: The Rust CLI must produce correct plans from real disk state. This test
# bridges unit tests (pure logic) with integration: real LUKS, real btrfs,
# real command output parsed by the Rust probe layer.
#
# Dependencies: bash braid (init-disk, apply) for pool setup; Rust braid-rust
# binary for plan validation.
{
  name = "braid-plan-rust";

  nodes.machine = { pkgs, ... }: let
    braid-cli = pkgs.writeShellApplication {
      name = "braid";
      runtimeInputs = [ pkgs.cryptsetup pkgs.btrfs-progs pkgs.util-linux pkgs.jq pkgs.coreutils ];
      text = builtins.readFile ../scripts/braid.sh;
    };

    braid-rust = pkgs.rustPlatform.buildRustPackage {
      pname = "braid-cli";
      version = "0.1.0";
      src = ../cli;
      cargoLock.lockFile = ../cli/Cargo.lock;
      postInstall = ''
        mv $out/bin/braid $out/bin/braid-rust
      '';
    };
  in {
    virtualisation.emptyDiskImages = [
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk2"; }
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk3"; }
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk4"; }
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

  testScript = builtins.readFile ./braid-plan-rust.py;
}
