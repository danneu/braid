# Test: auto-unlock-key-wrong
#
# What: Verifies that when autoUnlock is enabled and a USB device is
# present but contains a wrong/invalid keyfile, boot succeeds with the
# pool remaining locked.
#
# Why: A corrupted or swapped USB must not block boot, cause error loops,
# or leave the system in a degraded state.
{ braid }:
{ lib, pkgs, ... }:
let
  passphrase = "testpassphrase";
  diskKeys = [ "disk1" ];
in
{
  name = "auto-unlock-key-wrong";

  nodes.machine = { pkgs, ... }: {
    imports = [ ../../modules/braid ];

    braid = {
      enable = true;
      package = braid;
      disks = lib.genAttrs diskKeys (d: { byId = "/dev/disk/by-id/virtio-${d}"; });
      autoUnlock = {
        enable = true;
        keyDevice = "/dev/disk/by-id/virtio-usbkey";
        timeoutSec = 10;
      };
    };

    virtualisation.emptyDiskImages = [
      { size = 512; driveConfig.deviceExtraOpts.serial = "disk1"; }
      # USB with WRONG keyfile
      { size = 64; driveConfig.deviceExtraOpts.serial = "usbkey"; }
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
        "x-systemd.device-timeout=10s"
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
          pkgs.e2fsprogs
        ];

        services.prepare-fixture = {
          description = "Prepare LUKS + btrfs + wrong USB key fixture";
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
            pkgs.e2fsprogs
          ];
          script = ''
            set -eu

            for dev in /dev/disk/by-id/virtio-disk1 /dev/disk/by-id/virtio-usbkey; do
              i=0
              while [ "$i" -lt 100 ]; do
                [ -b "$dev" ] && break
                sleep 0.1
                i=$((i + 1))
              done
              test -b "$dev"
            done

            # LUKS format pool disk (no keyfile enrollment — that's the point)
            dev="/dev/disk/by-id/virtio-disk1"
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

            # Format USB with WRONG random keyfile (not enrolled in LUKS)
            usb="/dev/disk/by-id/virtio-usbkey"
            mkfs.ext4 -F "$usb"
            mkdir -p /tmp/usb-mnt
            mount "$usb" /tmp/usb-mnt
            dd if=/dev/urandom of=/tmp/usb-mnt/braid.key bs=4096 count=1 iflag=fullblock
            chmod 400 /tmp/usb-mnt/braid.key
            umount /tmp/usb-mnt
          '';
        };
      };
    };
  };

  testScript = builtins.readFile ./auto-unlock-key-wrong.py;
}
