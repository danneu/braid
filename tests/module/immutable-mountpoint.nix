# Test: immutable-mountpoint
#
# What: Verifies braid seals the offline pool mountpoint immutable (chattr +i)
# so writes while the pool is unmounted fail loudly with EPERM instead of
# silently landing on the root filesystem and being shadowed when the pool
# mounts. Covers the boot seal, persistence across unlock/lock, mount-over-
# immutable, the mounted-root safety guard, the STATX_ATTR_MOUNT_ROOT bind-mount
# predicate, the doctor detection signal, the explicit seal/unseal levers, the
# activation self-heal, and the seal-before-mount ordering on auto-unlock.
#
# Why: This is a data-safety invariant. A regression -- a dropped boot unit, a
# lost before-auto-unlock edge, a sealed live root, or a seal that does not
# survive lock -- silently reopens the unmounted-mountpoint bug.
# See docs/design/decisions/028-immutable-unmounted-mountpoint.md.
{ braid }:
{ pkgs, lib, ... }:
let
  passphrase = "testpassphrase";
  diskNames = [
    "disk1"
    "disk2"
  ];

  # Shared pool.json -- the initrd fixture bypasses `braid add`.
  poolJson = ''f /var/lib/braid/pool.json 0644 root root - {"disks":{"11111111-1111-1111-1111-111111111111":{"name":"disk1","by_id":"/dev/disk/by-id/virtio-disk1"},"22222222-2222-2222-2222-222222222222":{"name":"disk2","by_id":"/dev/disk/by-id/virtio-disk2"}}}'';
in
{
  name = "immutable-mountpoint";

  nodes = {
    # Manual-unlock node: pool is OFFLINE after boot (cases 1-8 and 10 depend on
    # the boot seal running in the offline window).
    machine =
      { pkgs, lib, ... }:
      {
        imports = [
          ../../modules/braid
          (import ./lib/initrd-fixture.nix {
            inherit passphrase diskNames;
            description = "Prepare LUKS + btrfs fixture for immutable-mountpoint tests";
          })
        ];

        braid = {
          enable = true;
          package = braid;
        };

        systemd.tmpfiles.rules = [
          "d /var/lib/braid 0755 root root -"
          poolJson
        ];

        # Override braid-unlock.service to avoid the interactive
        # systemd-ask-password prompt -- VM tests have no TTY agent.
        systemd.services.braid-unlock.script = lib.mkForce ''
          printf '%s\n' '${passphrase}' | braid unlock --passphrase-stdin
        '';

        virtualisation.emptyDiskImages = [
          {
            size = 512;
            driveConfig.deviceExtraOpts.serial = "disk1";
          }
          {
            size = 512;
            driveConfig.deviceExtraOpts.serial = "disk2";
          }
        ];
        virtualisation.memorySize = 2048;

        # lsattr / chattr for asserting and simulating the immutable attribute.
        environment.systemPackages = [
          pkgs.btrfs-progs
          pkgs.cryptsetup
          pkgs.e2fsprogs
        ];
      };

    # Auto-unlock node: pool comes ONLINE at boot via braid-auto-unlock.service
    # (USB key present). Case 9 needs the system that never boots offline to
    # prove the seal-before-mount ordering. autoUnlock is build-time, so it
    # cannot share the manual node.
    autoMachine =
      { pkgs, lib, ... }:
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
              usb="/dev/disk/by-id/virtio-usbkey"
              mkfs.ext4 -F "$usb"
              mkdir -p /tmp/usb-mnt
              mount "$usb" /tmp/usb-mnt
              dd if=/dev/urandom of=/tmp/usb-mnt/braid.key bs=4096 count=1 iflag=fullblock
              chmod 400 /tmp/usb-mnt/braid.key
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
          autoUnlock = {
            enable = true;
            keyDevice = "/dev/disk/by-id/virtio-usbkey";
            timeoutSec = 10;
          };
        };

        systemd.tmpfiles.rules = [
          "d /var/lib/braid 0755 root root -"
          poolJson
        ];

        virtualisation.emptyDiskImages = [
          {
            size = 512;
            driveConfig.deviceExtraOpts.serial = "disk1";
          }
          {
            size = 512;
            driveConfig.deviceExtraOpts.serial = "disk2";
          }
          {
            size = 64;
            driveConfig.deviceExtraOpts.serial = "usbkey";
          }
        ];
        virtualisation.memorySize = 2048;

        environment.systemPackages = [
          pkgs.btrfs-progs
          pkgs.cryptsetup
          pkgs.e2fsprogs
        ];

        # Re-declare the USB mount for VM compat (virtualisation.fileSystems
        # uses mkVMOverride, which replaces all fileSystems entries).
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
  };

  testScript = builtins.readFile ./immutable-mountpoint.py;
}
