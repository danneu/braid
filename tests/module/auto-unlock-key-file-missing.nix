# Test: auto-unlock-key-file-missing
#
# Intent: Verify that auto-unlock skips cleanly when the USB is present but
# braid.key is missing.
#
# Why it exists: The realpath -e missing-file path must still unmount via the
# EXIT trap, leave the pool locked, and keep boot healthy.
#
# Scenario: NixOS module test with an ext4 "USB" disk that contains no
# braid.key. The auto-unlock service mounts the USB, cannot resolve the
# keyfile, unmounts, and exits successfully.
{ braid }:
{ pkgs, ... }:
let
  passphrase = "testpassphrase";
  diskNames = [ "disk1" ];
in
{
  name = "auto-unlock-key-file-missing";

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
          description = "Prepare LUKS + btrfs + empty USB key fixture";
          postScript = ''
            usb="/dev/disk/by-id/virtio-usbkey"
            mkfs.ext4 -F "$usb"
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

  testScript = builtins.readFile ./auto-unlock-key-file-missing.py;
}
