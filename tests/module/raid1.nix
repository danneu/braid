# Test: braid-module-raid1
#
# What: Enables the braid module with 3 disks. An initrd fixture pre-formats
# the empty virtual drives as LUKS + btrfs RAID1. After boot, the test script
# unlocks the pool via `braid unlock --passphrase-stdin`. Validates the mount,
# RAID1 profile, and runtime config.
#
# Why: Validates the module's storage path with the production-like 3-disk
# RAID1 configuration — braid-unlock opening all 3 LUKS devices,
# btrfs-device-scan gating the mount, and correct RAID1 profile on the pool.
#
# Dependencies: braid-module-single-disk (single-disk path works),
# hello-world (VM infra).
{ braid }:
{ lib, pkgs, ... }:
let
  passphrase = "testpassphrase";
  diskNames = [
    "disk1"
    "disk2"
    "disk3"
  ];
in
{
  name = "braid-module-raid1";

  nodes.machine =
    { pkgs, ... }:
    {
      imports = [
        ../../modules/braid
        (import ./lib/initrd-fixture.nix {
          inherit passphrase diskNames;
          description = "Prepare LUKS + btrfs RAID1 fixture";
        })
      ];

      braid = {
        enable = true;
        package = braid;
        disks = lib.genAttrs diskNames (d: {
          byId = "/dev/disk/by-id/virtio-${d}";
        });
      };

      virtualisation.emptyDiskImages = [
        {
          size = 256;
          driveConfig.deviceExtraOpts.serial = "disk1";
        }
        {
          size = 256;
          driveConfig.deviceExtraOpts.serial = "disk2";
        }
        {
          size = 256;
          driveConfig.deviceExtraOpts.serial = "disk3";
        }
      ];
      virtualisation.memorySize = 2048;

      environment.systemPackages = [ pkgs.btrfs-progs ];

      # Re-declare mount for VM compat (qemu-vm.nix clobbers fileSystems)
      virtualisation.fileSystems."/mnt/storage" = {
        device = "/dev/mapper/braid-disk1";
        fsType = "btrfs";
        options = [
          "degraded"
          "nofail"
          "noatime"
          "skip_balance"
          "x-systemd.device-timeout=1s"
          "x-systemd.requires=btrfs-device-scan.service"
          "x-systemd.after=btrfs-device-scan.service"
        ];
      };

    };

  testScript = builtins.readFile ./raid1.py;
}
