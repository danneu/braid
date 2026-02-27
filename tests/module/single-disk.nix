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

        # Fixture: format the empty drive as LUKS + single-disk btrfs
        # before switch-root. Stage-2 braid-unlock opens it.
        services.prepare-luks-btrfs-fixture = {
          description = "Prepare LUKS + btrfs fixture (single disk)";
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
          '';
        };
      };
    };
  };

  testScript = builtins.readFile ./single-disk.py;
}
