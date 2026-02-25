# Test: replace-failed-disk
#
# What: Server boots with 3 LUKS-encrypted drives as btrfs RAID1, but disk3 is
# bricked (simulating drive death). The server boots degraded via initrd SSH
# unlock, then `braid replace --old disk3 --new disk4` replaces the dead drive
# with a fresh disk4. The pool returns to healthy 3-drive RAID1 with all data
# intact.
#
# Why: This is the scariest real-world scenario — a drive dies, you boot
# degraded, and you need to replace it without reinstalling. It crosses every
# integration boundary: initrd SSH, degraded btrfs, and the intent CLI. No
# other test covers this full recovery cycle.
#
# Dependencies: degraded-boot (initrd SSH + degraded mount), braid add/replace.
{ braid }:
{ lib, pkgs, ... }:
let
  passphrase = "testpassphrase";
  initrdSshFixtureDir = pkgs.path + "/nixos/tests/initrd-network-ssh";
  disks = [ "disk1" "disk2" "disk3" ];
  mapperNames = map (d: "braid-${d}") disks;

  # systemd escapes "-" as "\x2d" in unit instance names.
  # Without this, dependencies silently don't match and the fixture never runs.
  cryptsetupUnit = name:
    "systemd-cryptsetup@${builtins.replaceStrings ["-"] ["\\x2d"] name}.service";
in
{
  name = "replace-failed-disk";

  nodes.server = { config, pkgs, ... }: {
    virtualisation.emptyDiskImages = [
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk2"; }
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk3"; }
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk4"; }
    ];
    virtualisation.memorySize = 2048;

    environment.systemPackages = [
      braid
      pkgs.cryptsetup
      pkgs.btrfs-progs
    ];

    environment.etc."braid/config.json".text = builtins.toJSON {
      disks = {
        disk1 = { by_id = "/dev/disk/by-id/virtio-disk1"; };
        disk2 = { by_id = "/dev/disk/by-id/virtio-disk2"; };
        disk3 = { by_id = "/dev/disk/by-id/virtio-disk3"; };
        disk4 = { by_id = "/dev/disk/by-id/virtio-disk4"; };
      };
      mount_point = "/mnt/storage";
    };

    # Mount in initrd with degraded option.
    virtualisation.fileSystems."/mnt/storage" = {
      device = "/dev/mapper/braid-disk1";
      fsType = "btrfs";
      neededForBoot = true;
      options = [
        "degraded"
        "x-systemd.requires=btrfs-device-scan.service"
        "x-systemd.after=btrfs-device-scan.service"
      ];
    };

    # Stage-2 copy of btrfs-device-scan.
    systemd.services.btrfs-device-scan = {
      description = "Scan for btrfs multi-device filesystems";
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
      };
      path = [ pkgs.btrfs-progs ];
      script = "btrfs device scan";
    };

    boot.kernelParams = [
      "ip=${config.networking.primaryIPAddress}:::255.255.255.0::eth1:none"
    ];

    boot.initrd = {
      supportedFilesystems = [ "btrfs" ];

      systemd = {
        enable = true;
        network.enable = true;
        users.root.shell = "/bin/sh";
        extraBin = {
          cryptsetup = "${pkgs.cryptsetup}/bin/cryptsetup";
        };
        storePaths = [
          pkgs.cryptsetup
          pkgs.btrfs-progs
          pkgs.util-linux
        ];

        # One-shot initrd fixture: format empty drives as LUKS + btrfs RAID1,
        # write test data, then brick disk3's LUKS header to simulate drive death.
        services.prepare-luks-btrfs-fixture = {
          description = "Prepare LUKS + btrfs RAID1 fixture with bricked disk3";
          requiredBy = map cryptsetupUnit mapperNames;
          before = [ "cryptsetup-pre.target" ]
            ++ map cryptsetupUnit mapperNames;
          after = [ "systemd-udevd.service" ];
          unitConfig.DefaultDependencies = false;
          serviceConfig = {
            Type = "oneshot";
            RemainAfterExit = true;
          };
          path = [
            pkgs.coreutils
            pkgs.cryptsetup
            pkgs.btrfs-progs
            pkgs.util-linux
          ];
          script = ''
            set -eu

            # Wait for all drives and LUKS-format them
            for disk in ${lib.concatStringsSep " " disks}; do
              dev="/dev/disk/by-id/virtio-$disk"
              i=0
              while [ "$i" -lt 100 ]; do
                [ -b "$dev" ] && break
                sleep 0.1
                i=$((i + 1))
              done
              test -b "$dev"

              if ! cryptsetup isLuks "$dev" 2>/dev/null; then
                echo -n '${passphrase}' | cryptsetup luksFormat --batch-mode --key-file=- --pbkdf pbkdf2 --pbkdf-force-iterations 1000 "$dev"
              fi
            done

            # Open with -fmt suffix to avoid triggering systemd device/mount
            # units. Using the real names would make /dev/mapper/braid-disk1
            # appear, which could trigger dependent units prematurely.
            for disk in ${lib.concatStringsSep " " disks}; do
              echo -n '${passphrase}' | cryptsetup luksOpen --key-file=- "/dev/disk/by-id/virtio-$disk" "braid-$disk-fmt"
            done

            # Create btrfs RAID1 across all drives
            if ! btrfs filesystem show /dev/mapper/braid-disk1-fmt >/dev/null 2>&1; then
              mkfs.btrfs -f -d raid1 -m raid1 ${lib.concatMapStringsSep " " (d: "/dev/mapper/braid-${d}-fmt") disks}
            fi

            # Mount and write test data before bricking disk3
            mkdir -p /tmp/fixture-mount
            mount /dev/mapper/braid-disk1-fmt /tmp/fixture-mount
            echo 'data written before drive death' > /tmp/fixture-mount/survived.txt
            sync
            umount /tmp/fixture-mount

            # Close all — the real cryptsetup units will reopen them
            for disk in ${lib.concatStringsSep " " disks}; do
              cryptsetup luksClose "braid-$disk-fmt"
            done

            # Brick disk3 — zero the LUKS header so cryptsetup fails on it
            dd if=/dev/zero of=/dev/disk/by-id/virtio-disk3 bs=1M count=10
          '';
        };

        # After LUKS devices are unlocked, scan for btrfs devices.
        services.btrfs-device-scan = {
          description = "Scan for btrfs multi-device filesystems";
          after = map cryptsetupUnit mapperNames;
          wants = map cryptsetupUnit mapperNames;
          before = [ "initrd-fs.target" ];
          wantedBy = [ "initrd-fs.target" ];
          unitConfig.DefaultDependencies = false;
          serviceConfig = {
            Type = "oneshot";
            RemainAfterExit = true;
          };
          path = [ pkgs.btrfs-progs ];
          script = "btrfs device scan";
        };
      };

      # LUKS devices with nofail + timeout so a dead drive doesn't block boot.
      luks.devices = lib.mkVMOverride (
        lib.genAttrs mapperNames (name: {
          device = "/dev/disk/by-id/virtio-${lib.removePrefix "braid-" name}";
          crypttabExtraOpts = [ "nofail" "x-systemd.device-timeout=10s" ];
        })
      );

      network = {
        enable = true;
        ssh = {
          enable = true;
          port = 2222;
          authorizedKeys = [ (builtins.readFile (initrdSshFixtureDir + "/id_ed25519.pub")) ];
          hostKeys = [ (initrdSshFixtureDir + "/ssh_host_ed25519_key") ];
        };
      };
    };
  };

  nodes.client = { pkgs, ... }: {
    environment.etc."ssh/test_id_ed25519" = {
      source = initrdSshFixtureDir + "/id_ed25519";
      mode = "0600";
    };
  };

  testScript = builtins.readFile ./replace-failed-disk.py;
}
