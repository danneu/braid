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
  diskNames = [
    "disk1"
    "disk2"
  ];
in
{
  name = "auto-unlock-key-present";

  nodes.machine =
    { pkgs, ... }:
    {
      imports = [
        ../../modules/braid
        (import ./lib/initrd-fixture.nix {
          inherit passphrase diskNames;
          extraWaitDevices = [ "/dev/disk/by-id/virtio-usbkey" ];
          extraStorePaths = [ pkgs.e2fsprogs ];
          extraPath = [ pkgs.e2fsprogs ];
          description = "Prepare LUKS + btrfs + USB key fixture";
          postScript = ''
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
          '';
        })
      ];

      braid = {
        enable = true;
        package = braid;
        disks = lib.genAttrs diskNames (d: {
          byId = "/dev/disk/by-id/virtio-${d}";
        });
        autoUnlock = {
          enable = true;
          keyDevice = "/dev/disk/by-id/virtio-usbkey";
          timeoutSec = 10;
        };
      };

      virtualisation.emptyDiskImages = [
        {
          size = 512;
          driveConfig.deviceExtraOpts.serial = "disk1";
        }
        {
          size = 512;
          driveConfig.deviceExtraOpts.serial = "disk2";
        }
        # "USB" key device
        {
          size = 64;
          driveConfig.deviceExtraOpts.serial = "usbkey";
        }
      ];
      virtualisation.memorySize = 2048;

      environment.systemPackages = [
        pkgs.btrfs-progs
        pkgs.cryptsetup
      ];

      # Re-declare mounts for VM compat (virtualisation.fileSystems uses
      # mkVMOverride which replaces all fileSystems entries, so entries
      # from the braid module must be re-declared here).
      virtualisation.fileSystems."/run/braid-key" = {
        device = "/dev/disk/by-id/virtio-usbkey";
        fsType = "auto";
        options = [
          "ro"
          "nosuid"
          "nodev"
          "noexec"
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

    };

  testScript = builtins.readFile ./auto-unlock-key-present.py;
}
