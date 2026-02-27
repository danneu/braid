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
  diskNames = [ "disk1" "disk2" "disk3" ];
in
{
  name = "braid-module-raid1";

  nodes.machine = { pkgs, ... }: {
    imports = [ ../../modules/braid ];

    braid = {
      enable = true;
      package = braid;
      disks = lib.genAttrs diskNames (d: { byId = "/dev/disk/by-id/virtio-${d}"; });
    };

    virtualisation.emptyDiskImages = [
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk2"; }
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk3"; }
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

    boot.initrd = {
      kernelModules = [ "dm-crypt" ];

      systemd.enable = true;
      systemd = {
        storePaths = [
          pkgs.cryptsetup
          pkgs.btrfs-progs
          pkgs.util-linux
        ];

        # Fixture: format empty drives as LUKS + btrfs RAID1
        # before switch-root. Stage-2 braid-unlock opens them.
        services.prepare-luks-btrfs-fixture = {
          description = "Prepare LUKS + btrfs RAID1 fixture";
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

            for disk in ${lib.concatStringsSep " " diskNames}; do
              dev="/dev/disk/by-id/virtio-$disk"
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
            done

            for disk in ${lib.concatStringsSep " " diskNames}; do
              echo -n '${passphrase}' | cryptsetup luksOpen --key-file=- \
                "/dev/disk/by-id/virtio-$disk" "braid-$disk-fmt"
            done

            if ! btrfs filesystem show /dev/mapper/braid-disk1-fmt >/dev/null 2>&1; then
              mkfs.btrfs -f -d raid1 -m raid1 \
                ${lib.concatMapStringsSep " " (d: "/dev/mapper/braid-${d}-fmt") diskNames}
            fi

            for disk in ${lib.concatStringsSep " " diskNames}; do
              cryptsetup luksClose "braid-$disk-fmt"
            done
          '';
        };
      };
    };
  };

  testScript = builtins.readFile ./raid1.py;
}
