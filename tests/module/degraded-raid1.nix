# Test: braid-module-degraded-raid1
#
# What: Enables the braid module with 3 disks. An initrd fixture formats all 3
# as LUKS + btrfs RAID1, writes test data, then bricks disk3's LUKS header.
# The module's nofail/wants/degraded defaults let disk3's cryptsetup fail
# without cascading. The VM boots with the pool mounted degraded (2 of 3),
# test data survives, and new writes work.
#
# Why: Validates the "one drive dead" tier of graceful failure using the module's
# own defaults — no manual overrides needed. This is the most common failure
# scenario: a single drive dies in a RAID1 pool.
#
# Dependencies: braid-module-raid1 (happy-path RAID1 works),
# braid-module-bad-config (nofail boot-continue works).
{ braid }:
{ lib, pkgs, ... }:
let
  passphrase = "testpassphrase";
  keyFile = pkgs.writeText "luks-test-key" passphrase;
  diskKeys = [ "disk1" "disk2" "disk3" ];
  mapperNames = map (d: "braid-${d}") diskKeys;

  # systemd-cryptsetup-generator escapes hyphens in unit instance names.
  cryptsetupUnit = name:
    "systemd-cryptsetup@${builtins.replaceStrings ["-"] ["\\x2d"] name}.service";
in
{
  name = "braid-module-degraded-raid1";

  nodes.machine = { pkgs, ... }: {
    imports = [ ../../modules/braid ];

    braid = {
      enable = true;
      package = braid;
      disks = lib.genAttrs diskKeys (d: { byId = "/dev/disk/by-id/virtio-${d}"; });
    };

    virtualisation.emptyDiskImages = [
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk2"; }
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk3"; }
    ];
    virtualisation.memorySize = 2048;

    environment.systemPackages = [ pkgs.btrfs-progs ];

    # Re-declare mount for VM compat (qemu-vm.nix clobbers fileSystems)
    virtualisation.fileSystems."/mnt/storage" = {
      device = "/dev/mapper/braid-disk1";
      fsType = "btrfs";
      neededForBoot = true;
      options = [
        "degraded"
        "nofail"
        "x-systemd.requires=btrfs-device-scan.service"
        "x-systemd.after=btrfs-device-scan.service"
      ];
    };

    boot.initrd = {
      systemd = {
        storePaths = [
          keyFile
          pkgs.cryptsetup
          pkgs.btrfs-progs
          pkgs.util-linux
        ];

        # Fixture: format LUKS + btrfs RAID1, write test data, brick disk3
        services.prepare-luks-btrfs-fixture = {
          description = "Prepare LUKS + btrfs RAID1 fixture with bricked disk3";
          requiredBy = map cryptsetupUnit mapperNames;
          before = [ "cryptsetup-pre.target" ]
            ++ map cryptsetupUnit mapperNames;
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
            for disk in ${lib.concatStringsSep " " diskKeys}; do
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
            for disk in ${lib.concatStringsSep " " diskKeys}; do
              echo -n '${passphrase}' | cryptsetup luksOpen --key-file=- \
                "/dev/disk/by-id/virtio-$disk" "braid-$disk-fmt"
            done

            # Create btrfs RAID1 across all drives
            if ! btrfs filesystem show /dev/mapper/braid-disk1-fmt >/dev/null 2>&1; then
              mkfs.btrfs -f -d raid1 -m raid1 \
                ${lib.concatMapStringsSep " " (d: "/dev/mapper/braid-${d}-fmt") diskKeys}
            fi

            # Mount and write test data before bricking disk3
            mkdir -p /tmp/fixture-mount
            mount /dev/mapper/braid-disk1-fmt /tmp/fixture-mount
            echo 'data written before drive death' > /tmp/fixture-mount/survived.txt
            sync
            umount /tmp/fixture-mount

            # Close all — the real cryptsetup units will reopen them
            for disk in ${lib.concatStringsSep " " diskKeys}; do
              cryptsetup luksClose "braid-$disk-fmt"
            done

            # Brick disk3 — zero the LUKS header so cryptsetup fails on it
            dd if=/dev/zero of=/dev/disk/by-id/virtio-disk3 bs=1M count=10
          '';
        };
      };

      # Override module's luks.devices: add keyFile for auto-unlock in VM.
      luks.devices = lib.mkVMOverride (
        lib.genAttrs mapperNames (name: {
          device = "/dev/disk/by-id/virtio-${lib.removePrefix "braid-" name}";
          keyFile = "${keyFile}";
          crypttabExtraOpts = [ "nofail" "x-systemd.device-timeout=10s" ];
        })
      );
    };
  };

  testScript = builtins.readFile ./degraded-raid1.py;
}
