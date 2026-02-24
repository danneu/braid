# Test: braid-module-single-disk-dead
#
# What: Enables the braid module with a single disk. An initrd fixture formats
# it as LUKS + single-disk btrfs, then bricks the LUKS header. The module's
# nofail defaults let the cryptsetup failure pass without cascading. The VM
# boots to multi-user with no /mnt/storage (no RAID1 fallback).
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
  keyFile = pkgs.writeText "luks-test-key" passphrase;
  disks = [ "disk1" ];
  mapperNames = map (d: "virtio-${d}") disks;

  # systemd-cryptsetup-generator escapes hyphens in unit instance names.
  cryptsetupUnit = name:
    "systemd-cryptsetup@${builtins.replaceStrings ["-"] ["\\x2d"] name}.service";
in
{
  name = "braid-module-single-disk-dead";

  nodes.machine = { pkgs, ... }: {
    imports = [ ../../modules/braid ];

    braid = {
      enable = true;
      package = braid;
      disks = map (d: "/dev/disk/by-id/virtio-${d}") disks;
    };

    virtualisation.emptyDiskImages = [
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk1"; }
    ];
    virtualisation.memorySize = 2048;

    # Re-declare mount for VM compat (qemu-vm.nix clobbers fileSystems)
    virtualisation.fileSystems."/mnt/storage" = {
      device = "/dev/mapper/virtio-disk1";
      fsType = "btrfs";
      neededForBoot = true;
      options = [
        "degraded"
        "nofail"
        "x-systemd.requires=btrfs-device-scan.service"
        "x-systemd.after=btrfs-device-scan.service"
      ];
    };

    boot.initrd = {
      systemd = {
        storePaths = [
          keyFile
          pkgs.cryptsetup
          pkgs.btrfs-progs
          pkgs.util-linux
        ];

        # Fixture: format LUKS + btrfs, then brick the LUKS header
        services.prepare-luks-btrfs-fixture = {
          description = "Prepare LUKS + btrfs fixture then brick disk";
          requiredBy = map cryptsetupUnit mapperNames;
          before = [ "cryptsetup-pre.target" ]
            ++ map cryptsetupUnit mapperNames;
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
              "$dev" "virtio-disk1-fmt"

            if ! btrfs filesystem show /dev/mapper/virtio-disk1-fmt >/dev/null 2>&1; then
              mkfs.btrfs -f /dev/mapper/virtio-disk1-fmt
            fi

            cryptsetup luksClose "virtio-disk1-fmt"

            # Brick the disk — zero the LUKS header so cryptsetup fails
            dd if=/dev/zero of=/dev/disk/by-id/virtio-disk1 bs=1M count=10
          '';
        };
      };

      # Override module's luks.devices: add keyFile for auto-unlock in VM.
      luks.devices = lib.mkVMOverride (
        lib.genAttrs mapperNames (name: {
          device = "/dev/disk/by-id/virtio-${lib.removePrefix "virtio-" name}";
          keyFile = "${keyFile}";
          crypttabExtraOpts = [ "nofail" "x-systemd.device-timeout=10s" ];
        })
      );
    };
  };

  testScript = builtins.readFile ./05-single-disk-dead.py;
}
