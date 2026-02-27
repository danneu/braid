# Test: auto-unlock-key-present
#
# What: Verifies that when autoUnlock is enabled and a USB device with a
# valid keyfile is present at boot, the pool is automatically mounted and
# the USB is unmounted after use.
#
# Why: This is the primary auto-unlock use case. If systemd service ordering,
# mount unit config, or keyfile path resolution is wrong, users get a locked
# NAS after an unattended reboot.
{ braid }:
{ lib, pkgs, ... }:
let
  passphrase = "testpassphrase";
  diskNames = [ "disk1" "disk2" ];
in
{
  name = "auto-unlock-key-present";

  nodes.machine = { pkgs, ... }: {
    imports = [ ../../modules/braid ];

    braid = {
      enable = true;
      package = braid;
      disks = lib.genAttrs diskNames (d: { byId = "/dev/disk/by-id/virtio-${d}"; });
      autoUnlock = {
        enable = true;
        keyDevice = "/dev/disk/by-id/virtio-usbkey";
        timeoutSec = 10;
      };
    };

    virtualisation.emptyDiskImages = [
      { size = 512; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 512; driveConfig.deviceExtraOpts.serial = "disk2"; }
      # "USB" key device
      { size = 64; driveConfig.deviceExtraOpts.serial = "usbkey"; }
    ];
    virtualisation.memorySize = 2048;

    environment.systemPackages = [ pkgs.btrfs-progs pkgs.cryptsetup ];

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

        # Fixture: format pool disks as LUKS + btrfs, format usbkey as ext4
        # with enrolled keyfile, all before switch-root.
        services.prepare-fixture = {
          description = "Prepare LUKS + btrfs + USB key fixture";
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

            # Wait for devices
            for dev in /dev/disk/by-id/virtio-disk1 /dev/disk/by-id/virtio-disk2 /dev/disk/by-id/virtio-usbkey; do
              i=0
              while [ "$i" -lt 100 ]; do
                [ -b "$dev" ] && break
                sleep 0.1
                i=$((i + 1))
              done
              test -b "$dev"
            done

            # LUKS format both pool disks
            for disk in disk1 disk2; do
              dev="/dev/disk/by-id/virtio-$disk"
              if ! cryptsetup isLuks "$dev" 2>/dev/null; then
                echo -n '${passphrase}' | cryptsetup luksFormat --batch-mode \
                  --key-file=- --pbkdf pbkdf2 --pbkdf-force-iterations 1000 "$dev"
              fi
            done

            # Format USB key as ext4 and write a random keyfile
            usb="/dev/disk/by-id/virtio-usbkey"
            mkfs.ext4 -F "$usb"
            mkdir -p /tmp/usb-mnt
            mount "$usb" /tmp/usb-mnt
            dd if=/dev/urandom of=/tmp/usb-mnt/braid.key bs=4096 count=1 iflag=fullblock
            chmod 400 /tmp/usb-mnt/braid.key

            # Enroll the keyfile into both pool disks
            for disk in disk1 disk2; do
              dev="/dev/disk/by-id/virtio-$disk"
              echo -n '${passphrase}' | cryptsetup luksAddKey --key-slot 1 "$dev" /tmp/usb-mnt/braid.key
            done

            umount /tmp/usb-mnt

            # Open disks and create btrfs pool
            for disk in disk1 disk2; do
              dev="/dev/disk/by-id/virtio-$disk"
              echo -n '${passphrase}' | cryptsetup luksOpen --key-file=- "$dev" "braid-$disk-fmt"
            done

            mkfs.btrfs -f -d raid1 -m raid1 /dev/mapper/braid-disk1-fmt /dev/mapper/braid-disk2-fmt

            for disk in disk1 disk2; do
              cryptsetup luksClose "braid-$disk-fmt"
            done
          '';
        };
      };
    };
  };

  testScript = builtins.readFile ./auto-unlock-key-present.py;
}
