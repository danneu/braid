# Test: braid-module-raid1
#
# What: Enables the braid module with 3 disks. The module generates LUKS
# device config for all 3, btrfs-device-scan services, and the mount unit.
# An initrd fixture pre-formats the empty virtual drives as LUKS + btrfs
# RAID1. KeyFiles auto-unlock LUKS (no SSH needed). The VM boots to
# multi-user with /mnt/storage mounted as a RAID1 pool.
#
# Why: Validates the module's storage path with the production-like 3-disk
# RAID1 configuration — all 3 LUKS devices mapped, btrfs-device-scan gating
# the mount, and correct RAID1 profile on the pool.
#
# Dependencies: braid-module-single-disk (single-disk path works),
# hello-world (VM infra).
{ braid }:
{ lib, pkgs, ... }:
let
  passphrase = "testpassphrase";
  keyFile = pkgs.writeText "luks-test-key" passphrase;
  diskKeys = [ "disk1" "disk2" "disk3" ];
  mapperNames = map (d: "braid-${d}") diskKeys;

  # systemd-cryptsetup-generator escapes hyphens in unit instance names.
  cryptsetupUnit = name:
    "systemd-cryptsetup@${builtins.replaceStrings ["-"] ["\\x2d"] name}.service";
in
{
  name = "braid-module-raid1";

  nodes.machine = { pkgs, ... }: {
    imports = [ ../../modules/braid ];

    braid = {
      enable = true;
      package = braid;
      disks = lib.genAttrs diskKeys (d: { byId = "/dev/disk/by-id/virtio-${d}"; });
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
          pkgs.util-linux
        ];

        # Fixture: format empty drives as LUKS + btrfs RAID1
        # before the real cryptsetup units run.
        services.prepare-luks-btrfs-fixture = {
          description = "Prepare LUKS + btrfs RAID1 fixture";
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

            for disk in ${lib.concatStringsSep " " diskKeys}; do
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

            for disk in ${lib.concatStringsSep " " diskKeys}; do
              echo -n '${passphrase}' | cryptsetup luksOpen --key-file=- \
                "/dev/disk/by-id/virtio-$disk" "braid-$disk-fmt"
            done

            if ! btrfs filesystem show /dev/mapper/braid-disk1-fmt >/dev/null 2>&1; then
              mkfs.btrfs -f -d raid1 -m raid1 \
                ${lib.concatMapStringsSep " " (d: "/dev/mapper/braid-${d}-fmt") diskKeys}
            fi

            for disk in ${lib.concatStringsSep " " diskKeys}; do
              cryptsetup luksClose "braid-$disk-fmt"
            done
          '';
        };
      };

      # Override module's luks.devices: add keyFile for auto-unlock in VM.
      # mkVMOverride needed because qemu-vm.nix blanket-overrides luks.devices.
      luks.devices = lib.mkVMOverride (
        lib.genAttrs mapperNames (name: {
          device = "/dev/disk/by-id/virtio-${lib.removePrefix "braid-" name}";
          keyFile = "${keyFile}";
          crypttabExtraOpts = [ "nofail" "x-systemd.device-timeout=10s" ];
        })
      );
    };
  };

  testScript = builtins.readFile ./02-raid1.py;
}
