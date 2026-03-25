# Test: braid-module-add-bootstrap
#
# What: Enables the braid module with a single raw disk (no initrd fixture).
# The test script runs `braid add` to bootstrap the pool from scratch, then
# verifies that the wrapper sets mount point permissions to root:storage 2770.
#
# Why: braid add mounts from the Rust CLI, not through a systemd service.
# The wrapper-based permission fixup must cover this path. Without this test,
# a regression in the wrapper would silently leave the mount root as
# root:root 0755, blocking non-root access.
#
# Dependencies: braid-module-single-disk (module loads correctly),
# hello-world (VM infra).
{ braid }:
{ lib, pkgs, ... }:
{
  name = "braid-module-add-bootstrap";

  nodes.machine = { pkgs, ... }: {
    imports = [ ../../modules/braid ];

    braid = {
      enable = true;
      package = braid;
      disks.disk1 = { byId = "/dev/disk/by-id/virtio-disk1"; };
    };

    virtualisation.emptyDiskImages = [
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk1"; }
    ];
    virtualisation.memorySize = 2048;

    environment.systemPackages = [ pkgs.btrfs-progs ];

    # Re-declare mount for VM compat (qemu-vm.nix clobbers fileSystems).
    # nofail — braid add creates and mounts the pool; this entry is for
    # systemd awareness only.
    virtualisation.fileSystems."/mnt/storage" = {
      device = "/dev/mapper/braid-disk1";
      fsType = "btrfs";
      options = [
        "degraded"
        "nofail"
        "noatime"
        "skip_balance"
        "subvolid=5"
        "x-systemd.device-timeout=1s"
        "x-systemd.requires=btrfs-device-scan.service"
        "x-systemd.after=btrfs-device-scan.service"
      ];
    };
  };

  testScript = builtins.readFile ./add-bootstrap.py;
}
