# Test: braid-module-single-disk
#
# What: Enables the braid module with a single disk. An initrd fixture
# pre-formats the empty virtual drive as LUKS + single-disk btrfs. After boot,
# the test script unlocks the pool via `braid unlock --passphrase-stdin`.
#
# Why: Validates the module's core storage path — braid-unlock opening LUKS,
# stage-2 btrfs-device-scan, and the mount — on the simplest possible pool
# (one disk, no RAID1).
#
# Dependencies: braid-module-disabled (module loads without error),
# hello-world (VM infra).
{ braid }:
{ lib, pkgs, ... }:
let
  passphrase = "testpassphrase";
  diskNames = [ "disk1" ];
in
{
  name = "braid-module-single-disk";

  nodes.machine =
    { pkgs, ... }:
    {
      imports = [
        ../../modules/braid
        (import ./lib/initrd-fixture.nix {
          inherit passphrase diskNames;
          description = "Prepare LUKS + btrfs fixture (single disk)";
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
          "subvolid=5"
          "x-systemd.device-timeout=1s"
          "x-systemd.requires=btrfs-device-scan.service"
          "x-systemd.after=btrfs-device-scan.service"
        ];
      };

    };

  testScript = builtins.readFile ./single-disk.py;
}
