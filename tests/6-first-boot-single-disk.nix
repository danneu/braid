# Test: first-boot-single-disk
#
# What: End-to-end day-1 user story. A single LUKS-encrypted disk is formatted
# in initrd, the client SSH-unlocks it via dropbear, btrfs mounts, Samba comes
# up, and the client mounts the SMB share and does a write/read round-trip.
#
# Why: This is the most common boot cycle — one disk, remote unlock, Samba
# serving — and no existing test covers this integrated path. The remote-unlock
# test doesn't touch Samba. The samba test doesn't do initrd unlock. This test
# validates that all three subsystems (initrd SSH, btrfs mount, Samba) compose
# correctly on the simplest possible pool (single disk, no RAID1).
#
# Dependencies: remote-unlock (initrd SSH + LUKS), samba (SMB serving),
# hello-world (VM infra).
{ lib, pkgs, ... }:
let
  passphrase = "testpassphrase";
  initrdSshFixtureDir = pkgs.path + "/nixos/tests/initrd-network-ssh";
  disks = [ "disk1" ];
in
{
  name = "first-boot-single-disk";

  nodes.server = { config, pkgs, ... }: {
    virtualisation.emptyDiskImages = [
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk1"; }
    ];
    virtualisation.memorySize = 2048;

    environment.systemPackages = [ pkgs.btrfs-progs ];

    # Mount in initrd. Same x-systemd gate as remote-unlock: wait for
    # btrfs-device-scan so the mount doesn't fire before the LUKS device
    # is open and btrfs has re-scanned paths.
    virtualisation.fileSystems."/mnt/storage" = {
      device = "/dev/mapper/disk1";
      fsType = "btrfs";
      neededForBoot = true;
      options = [
        "x-systemd.requires=btrfs-device-scan.service"
        "x-systemd.after=btrfs-device-scan.service"
      ];
    };

    # Stage-2 copy — x-systemd.requires persists across switch-root.
    systemd.services.btrfs-device-scan = {
      description = "Scan for btrfs devices";
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
      };
      path = [ pkgs.btrfs-progs ];
      script = "btrfs device scan";
    };

    # Samba: serve /mnt/storage over SMB
    services.samba = {
      enable = true;
      settings = {
        storage = {
          path = "/mnt/storage";
          browseable = "yes";
          "read only" = "no";
          "guest ok" = "no";
          "force user" = "nas";
        };
      };
    };

    users.users.nas = {
      isNormalUser = true;
      description = "Samba share user";
    };

    networking.firewall.allowedTCPPorts = [ 445 ];

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

        # Initrd fixture: format the empty drive as LUKS + single-disk btrfs.
        services.prepare-luks-btrfs-fixture = {
          description = "Prepare LUKS + btrfs fixture (single disk)";
          requiredBy = map (d: "systemd-cryptsetup@${d}.service") disks;
          before = [ "cryptsetup-pre.target" ]
            ++ map (d: "systemd-cryptsetup@${d}.service") disks;
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

            dev="/dev/disk/by-id/virtio-disk1"
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

            echo -n '${passphrase}' | cryptsetup luksOpen --key-file=- "$dev" "disk1-fmt"

            if ! btrfs filesystem show /dev/mapper/disk1-fmt >/dev/null 2>&1; then
              mkfs.btrfs -f /dev/mapper/disk1-fmt
            fi

            cryptsetup luksClose "disk1-fmt"
          '';
        };

        # Re-scan after LUKS open so btrfs finds the device at its new path.
        services.btrfs-device-scan = {
          description = "Scan for btrfs devices";
          after = map (d: "systemd-cryptsetup@${d}.service") disks;
          requires = map (d: "systemd-cryptsetup@${d}.service") disks;
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

      luks.devices = lib.mkVMOverride (
        lib.genAttrs disks (name: {
          device = "/dev/disk/by-id/virtio-${name}";
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
    environment.systemPackages = [ pkgs.cifs-utils ];
  };

  testScript = builtins.readFile ./first-boot-single-disk.py;
}
