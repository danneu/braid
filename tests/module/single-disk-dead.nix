# Test: braid-module-single-disk-dead
#
# What: Enables the braid module with a single disk. An initrd fixture formats
# it as LUKS + single-disk btrfs, then bricks the LUKS header. After boot,
# `braid unlock` fails (bricked LUKS). The system boots to multi-user with
# no /mnt/storage (no RAID1 fallback).
#
# Why: Validates the "all drives dead" tier on the simplest config. With only
# one drive and no RAID1, a dead drive means total data loss — but the system
# must still boot so the user can SSH in and fix the config or replace the drive.
#
# Dependencies: braid-module-single-disk (single-disk happy path),
# braid-module-bad-config (nofail boot-continue works).
{ braid }:
{ lib, pkgs, ... }:
let
  passphrase = "testpassphrase";
  diskNames = [ "disk1" ];
in
{
  name = "braid-module-single-disk-dead";

  nodes.machine = { pkgs, ... }: {
    imports = [ ../../modules/braid ];

    braid = {
      enable = true;
      package = braid;
      disks = lib.genAttrs diskNames (d: { byId = "/dev/disk/by-id/virtio-${d}"; });
    };

    virtualisation.emptyDiskImages = [
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk1"; }
    ];
    virtualisation.memorySize = 2048;

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

    boot.initrd = {
      kernelModules = [ "dm-crypt" ];

      systemd.enable = true;
      systemd = {
        storePaths = [
          pkgs.cryptsetup
          pkgs.btrfs-progs
          pkgs.util-linux
        ];

        # Fixture: format LUKS + btrfs, then brick the LUKS header
        services.prepare-luks-btrfs-fixture = {
          description = "Prepare LUKS + btrfs fixture then brick disk";
          wantedBy = [ "initrd.target" ];
          before = [ "initrd.target" ];
          after = [ "systemd-udevd.service" ];
          unitConfig.DefaultDependencies = false;
          serviceConfig = { Type = "oneshot"; RemainAfterExit = true; };
          path = [
            pkgs.coreutils
            pkgs.cryptsetup
            pkgs.btrfs-progs
            pkgs.util-linux
          ];
          script = ''
            set -eu

            dev="/dev/disk/by-id/virtio-disk1"
            i=0
            while [ "$i" -lt 100 ]; do
              [ -b "$dev" ] && break
              sleep 0.1
              i=$((i + 1))
            done
            test -b "$dev"

            if ! cryptsetup isLuks "$dev" 2>/dev/null; then
              echo -n '${passphrase}' | cryptsetup luksFormat --batch-mode \
                --key-file=- --pbkdf pbkdf2 --pbkdf-force-iterations 1000 "$dev"
            fi

            echo -n '${passphrase}' | cryptsetup luksOpen --key-file=- \
              "$dev" "braid-disk1-fmt"

            if ! btrfs filesystem show /dev/mapper/braid-disk1-fmt >/dev/null 2>&1; then
              mkfs.btrfs -f /dev/mapper/braid-disk1-fmt
            fi

            cryptsetup luksClose "braid-disk1-fmt"

            # Brick the disk — zero the LUKS header so cryptsetup fails
            dd if=/dev/zero of=/dev/disk/by-id/virtio-disk1 bs=1M count=10
          '';
        };
      };
    };
  };

  testScript = builtins.readFile ./single-disk-dead.py;
}
