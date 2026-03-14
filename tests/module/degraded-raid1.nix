# Test: braid-module-degraded-raid1
#
# What: Enables the braid module with 3 disks. An initrd fixture formats all 3
# as LUKS + btrfs RAID1, writes test data, then bricks disk3's LUKS header.
# After boot, `braid unlock` opens the surviving 2 disks, skips the bricked
# one, and mounts the pool degraded. Test data survives, new writes work.
#
# Why: Validates the "one drive dead" tier of graceful failure. This is the most
# common failure scenario: a single drive dies in a RAID1 pool.
#
# Dependencies: braid-module-raid1 (happy-path RAID1 works),
# braid-module-bad-config (nofail boot-continue works).
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
  name = "braid-module-degraded-raid1";

  nodes.machine =
    { pkgs, ... }:
    {
      imports = [
        ../../modules/braid
        (import ./lib/initrd-fixture.nix {
          inherit passphrase diskNames;
          supportedFilesystems = [ "btrfs" ];
          description = "Prepare LUKS + btrfs RAID1 fixture with bricked disk3";
          preCloseScript = ''
            # Mount and write test data before bricking disk3
            mkdir -p /tmp/fixture-mount
            mount /dev/mapper/braid-disk1-fmt /tmp/fixture-mount
            echo 'data written before drive death' > /tmp/fixture-mount/survived.txt
            sync
            umount /tmp/fixture-mount
          '';
          postScript = ''
            # Brick disk3 — zero the LUKS header so cryptsetup fails on it
            dd if=/dev/zero of=/dev/disk/by-id/virtio-disk3 bs=1M count=10
          '';
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
          "x-systemd.device-timeout=1s"
          "x-systemd.requires=btrfs-device-scan.service"
          "x-systemd.after=btrfs-device-scan.service"
        ];
      };

    };

  testScript = builtins.readFile ./degraded-raid1.py;
}
