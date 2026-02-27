# Test: auto-unlock-key-missing
#
# What: Verifies that when autoUnlock is enabled but no USB device is
# present, boot succeeds normally with the pool locked.
#
# Why: Principle 1 (resilient by default). A missing USB key must NEVER
# block boot or cause systemd to enter degraded state.
{ braid }:
{ lib, pkgs, ... }:
let
  passphrase = "testpassphrase";
  diskKeys = [ "disk1" ];
in
{
  name = "auto-unlock-key-missing";

  nodes.machine = { pkgs, ... }: {
    imports = [ ../../modules/braid ];

    braid = {
      enable = true;
      package = braid;
      disks = lib.genAttrs diskKeys (d: { byId = "/dev/disk/by-id/virtio-${d}"; });
      autoUnlock = {
        enable = true;
        # Point at a device that does NOT exist in this VM
        keyDevice = "/dev/disk/by-id/virtio-usbkey";
        timeoutSec = 2;
      };
    };

    virtualisation.emptyDiskImages = [
      { size = 512; driveConfig.deviceExtraOpts.serial = "disk1"; }
      # No usbkey disk — that's the whole point
    ];
    virtualisation.memorySize = 2048;

    # Re-declare mounts for VM compat (virtualisation.fileSystems uses
    # mkVMOverride which replaces all fileSystems entries, so entries
    # from the braid module must be re-declared here).
    virtualisation.fileSystems."/run/braid-key" = {
      device = "/dev/disk/by-id/virtio-usbkey";
      fsType = "auto";
      options = [
        "ro" "nosuid" "nodev" "noexec"
        "nofail"
        "noauto"
        "x-systemd.device-timeout=2s"
      ];
    };

    virtualisation.fileSystems."/mnt/storage" = {
      device = "/dev/mapper/braid-disk1";
      fsType = "btrfs";
      neededForBoot = false;
      options = [
        "degraded"
        "nofail"
        "x-systemd.device-timeout=1s"
        "x-systemd.requires=btrfs-device-scan.service"
        "x-systemd.after=btrfs-device-scan.service"
      ];
    };

    boot.initrd = {
      # The fixture needs dm-crypt for luksOpen/luksFormat.
      kernelModules = [ "dm-crypt" ];

      systemd.enable = true;
      systemd = {
        storePaths = [
          pkgs.cryptsetup
          pkgs.btrfs-progs
          pkgs.util-linux
        ];

        services.prepare-fixture = {
          description = "Prepare LUKS + btrfs fixture";
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

  testScript = builtins.readFile ./auto-unlock-key-missing.py;
}
