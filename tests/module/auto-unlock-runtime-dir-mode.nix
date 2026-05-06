# Test: auto-unlock-runtime-dir-mode
#
# Intent: Verify that /run/braid-key remains 0700 root:root while the USB is
# mounted at /run/braid-key/mnt, so non-root users cannot traverse to it.
#
# Why it exists: The auto-unlock key is plaintext while mounted. A permissive
# USB filesystem root must not make the key reachable during that mount window.
#
# Scenario: NixOS module test with a vfat "USB" disk. The test starts the mount
# unit directly, observes the mounted state, verifies the locked parent blocks
# nobody from listing either the parent or child path, then stops the unit.
{ braid }:
{ pkgs, ... }:
let
  passphrase = "testpassphrase";
  diskNames = [ "disk1" ];
in
{
  name = "auto-unlock-runtime-dir-mode";

  nodes.machine =
    { pkgs, ... }:
    {
      imports = [
        ../../modules/braid
        (import ./lib/initrd-fixture.nix {
          inherit passphrase diskNames;
          extraWaitDevices = [ "/dev/disk/by-id/virtio-usbkey" ];
          extraStorePaths = [ pkgs.dosfstools ];
          extraPath = [ pkgs.dosfstools ];
          description = "Prepare LUKS + btrfs + vfat USB fixture";
          postScript = ''
            usb="/dev/disk/by-id/virtio-usbkey"
            mkfs.vfat -F 32 "$usb"
          '';
        })
      ];

      boot.supportedFilesystems = [ "vfat" ];

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

      environment.systemPackages = [
        pkgs.util-linux
      ];

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

  testScript = builtins.readFile ./auto-unlock-runtime-dir-mode.py;
}
