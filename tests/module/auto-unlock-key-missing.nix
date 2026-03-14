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
  diskNames = [ "disk1" ];
in
{
  name = "auto-unlock-key-missing";

  nodes.machine =
    { pkgs, ... }:
    {
      imports = [
        ../../modules/braid
        (import ./lib/initrd-fixture.nix {
          inherit passphrase diskNames;
          description = "Prepare LUKS + btrfs fixture";
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
          # Point at a device that does NOT exist in this VM
          keyDevice = "/dev/disk/by-id/virtio-usbkey";
          timeoutSec = 2;
        };
      };

      virtualisation.emptyDiskImages = [
        {
          size = 512;
          driveConfig.deviceExtraOpts.serial = "disk1";
        }
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
          "ro"
          "nosuid"
          "nodev"
          "noexec"
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

    };

  testScript = builtins.readFile ./auto-unlock-key-missing.py;
}
