# Test: auto-unlock-key-wrong
#
# What: Verifies that when autoUnlock is enabled and a USB device is
# present but contains a wrong/invalid keyfile, boot succeeds with the
# pool remaining locked.
#
# Why: A corrupted or swapped USB must not block boot, cause error loops,
# or leave the system in a degraded state.
{ braid }:
{ pkgs, ... }:
let
  passphrase = "testpassphrase";
  diskNames = [ "disk1" ];
in
{
  name = "auto-unlock-key-wrong";

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
          description = "Prepare LUKS + btrfs + wrong USB key fixture";
          postScript = ''
            # Format USB with WRONG random keyfile (not enrolled in LUKS)
            usb="/dev/disk/by-id/virtio-usbkey"
            mkfs.ext4 -F "$usb"
            mkdir -p /tmp/usb-mnt
            mount "$usb" /tmp/usb-mnt
            dd if=/dev/urandom of=/tmp/usb-mnt/braid.key bs=4096 count=1 iflag=fullblock
            chmod 400 /tmp/usb-mnt/braid.key
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

      # Seed pool.json — the initrd fixture bypasses `braid add`, so there is
      # no pool membership file.  braid unlock requires it.
      systemd.tmpfiles.rules = [
        "d /var/lib/braid 0755 root root -"
        ''f /var/lib/braid/pool.json 0644 root root - {"disks":{"11111111-1111-1111-1111-111111111111":{"name":"disk1","by_id":"/dev/disk/by-id/virtio-disk1"}}}''
      ];

      virtualisation.emptyDiskImages = [
        {
          size = 512;
          driveConfig.deviceExtraOpts.serial = "disk1";
        }
        # USB with WRONG keyfile
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

  testScript = builtins.readFile ./auto-unlock-key-wrong.py;
}
