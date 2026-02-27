# Test: no-silent-degraded
#
# What: Verifies that a missing device does NOT get silently mounted degraded
# by the systemd mount unit. The fileSystems entry must not include 'degraded',
# so a raw `mount` of the pool with a missing member fails. Only `braid unlock`
# should mount degraded, because it detects the missing device and explicitly
# passes `-o degraded`.
#
# Why: When btrfs mounts RAID1 degraded, new block groups use `single` profile
# (one copy, zero redundancy). If systemd silently mounts degraded via fstab,
# the user never knows they're running unprotected. This is a data-loss time
# bomb — if another drive fails, single-profile data is gone. Forcing the user
# through `braid unlock` gives visibility and ensures the degraded state is a
# deliberate, informed decision.
#
# Scenario: 3-disk RAID1 pool, disk3's LUKS header is bricked. After boot,
# LUKS is opened on disk1+disk2 and btrfs is scanned. A direct mount (without
# -o degraded) must fail. Then `braid unlock` succeeds by adding `-o degraded`
# dynamically. Data written before the failure survives.
#
# Dependencies: braid-module-degraded-raid1 (degraded mount via braid works).
{ braid }:
{ lib, pkgs, ... }:
let
  passphrase = "testpassphrase";
  diskNames = [ "disk1" "disk2" "disk3" ];
in
{
  name = "no-silent-degraded";

  nodes.machine = { pkgs, ... }: {
    imports = [ ../../modules/braid ];

    braid = {
      enable = true;
      package = braid;
      disks = lib.genAttrs diskNames (d: { byId = "/dev/disk/by-id/virtio-${d}"; });
    };

    virtualisation.emptyDiskImages = [
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk2"; }
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk3"; }
    ];
    virtualisation.memorySize = 2048;

    environment.systemPackages = [ pkgs.btrfs-progs pkgs.cryptsetup ];

    # Re-declare mount for VM compat (qemu-vm.nix clobbers fileSystems).
    # Crucially: NO 'degraded' option. This matches the production module's
    # fileSystems entry, which must not include 'degraded'.
    virtualisation.fileSystems."/mnt/storage" = {
      device = "/dev/mapper/braid-disk1";
      fsType = "btrfs";
      options = [
        "nofail"
        "x-systemd.device-timeout=1s"
        "x-systemd.requires=btrfs-device-scan.service"
        "x-systemd.after=btrfs-device-scan.service"
      ];
    };

    boot.initrd = {
      supportedFilesystems = [ "btrfs" ];
      kernelModules = [ "dm-crypt" ];

      systemd.enable = true;
      systemd = {
        storePaths = [
          pkgs.cryptsetup
          pkgs.btrfs-progs
          pkgs.util-linux
        ];

        # Fixture: format LUKS + btrfs RAID1, write test data, brick disk3
        services.prepare-luks-btrfs-fixture = {
          description = "Prepare LUKS + btrfs RAID1 fixture with bricked disk3";
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

            # Wait for all drives and LUKS-format them
            for disk in ${lib.concatStringsSep " " diskNames}; do
              dev="/dev/disk/by-id/virtio-$disk"
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
            done

            # Open with -fmt suffix to avoid triggering systemd units
            for disk in ${lib.concatStringsSep " " diskNames}; do
              echo -n '${passphrase}' | cryptsetup luksOpen --key-file=- \
                "/dev/disk/by-id/virtio-$disk" "braid-$disk-fmt"
            done

            # Create btrfs RAID1 across all drives
            if ! btrfs filesystem show /dev/mapper/braid-disk1-fmt >/dev/null 2>&1; then
              mkfs.btrfs -f -d raid1 -m raid1 \
                ${lib.concatMapStringsSep " " (d: "/dev/mapper/braid-${d}-fmt") diskNames}
            fi

            # Mount and write test data before bricking disk3
            mkdir -p /tmp/fixture-mount
            mount /dev/mapper/braid-disk1-fmt /tmp/fixture-mount
            echo 'data written before drive death' > /tmp/fixture-mount/survived.txt
            sync
            umount /tmp/fixture-mount

            # Close all
            for disk in ${lib.concatStringsSep " " diskNames}; do
              cryptsetup luksClose "braid-$disk-fmt"
            done

            # Brick disk3 — zero the LUKS header so cryptsetup fails on it
            dd if=/dev/zero of=/dev/disk/by-id/virtio-disk3 bs=1M count=10
          '';
        };
      };
    };
  };

  testScript = builtins.readFile ./no-silent-degraded.py;
}
