# Test: auto-unlock-key-file-symlink
#
# Intent: Verify that auto-unlock refuses a braid.key symlink that resolves
# outside the USB mount root.
#
# Why it exists: The USB filesystem is attacker-controlled. A symlink escape
# must not let auto-unlock read a host file as key material.
#
# Scenario: NixOS module test with an ext4 "USB" disk whose braid.key is a
# symlink to /etc/shadow. The auto-unlock service resolves the path, refuses
# the escape, unmounts, and exits successfully.
{ braid }:
{ pkgs, ... }:
let
  passphrase = "testpassphrase";
  diskNames = [ "disk1" ];
in
{
  name = "auto-unlock-key-file-symlink";

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
          description = "Prepare LUKS + btrfs + symlink USB key fixture";
          postScript = ''
            usb="/dev/disk/by-id/virtio-usbkey"
            mkfs.ext4 -F "$usb"
            mkdir -p /tmp/usb-mnt
            mount "$usb" /tmp/usb-mnt
            ln -s /etc/shadow /tmp/usb-mnt/braid.key
            umount /tmp/usb-mnt
          '';
        })
      ];

      braid = {
        enable = true;
        package = braid;
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
          size = 64;
          driveConfig.deviceExtraOpts.serial = "usbkey";
        }
      ];
      virtualisation.memorySize = 2048;

      # Re-declare mounts for VM compat (virtualisation.fileSystems uses
      # mkVMOverride which replaces all fileSystems entries, so entries
      # from the braid module must be re-declared here).
      virtualisation.fileSystems."/run/braid-key/mnt" = {
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

    };

  testScript = builtins.readFile ./auto-unlock-key-file-symlink.py;
}
