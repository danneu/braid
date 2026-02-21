# Test: replace-failed-disk
#
# What: Server boots with 3 LUKS-encrypted drives as btrfs RAID1, but disk3 is
# bricked (simulating drive death). The server boots degraded via initrd SSH
# unlock, then braid-add-disk replaces the dead drive with a fresh disk4. The
# pool returns to healthy 3-drive RAID1 with all data intact.
#
# Why: This is the scariest real-world scenario — a drive dies, you boot
# degraded, and you need to replace it without reinstalling. It crosses every
# integration boundary: initrd SSH, degraded btrfs, and braid-add-disk. No
# other test covers this full recovery cycle.
#
# Dependencies: degraded-boot (initrd SSH + degraded mount), braid-add-disk
# (LUKS format + pool expansion).
#
# Changes from degraded-boot:
# 1. A 4th virtual disk (disk4) as the replacement drive
# 2. braid-add-disk + cryptsetup in environment.systemPackages
# 3. No Samba — this test focuses on the replacement cycle
{ lib, pkgs, ... }:
let
  passphrase = "testpassphrase";
  initrdSshFixtureDir = pkgs.path + "/nixos/tests/initrd-network-ssh";
  disks = [ "disk1" "disk2" "disk3" ];
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

    environment.systemPackages = let
      braid-cli = pkgs.writeShellApplication {
        name = "braid";
        runtimeInputs = [ pkgs.cryptsetup pkgs.btrfs-progs pkgs.util-linux pkgs.jq pkgs.coreutils ];
        text = builtins.readFile ../scripts/braid.sh;
      };
    in [
      braid-cli
      (pkgs.writeShellApplication {
        name = "braid-add-disk";
        runtimeInputs = [ braid-cli pkgs.cryptsetup pkgs.btrfs-progs pkgs.util-linux pkgs.jq ];
        text = builtins.readFile ../scripts/braid-add-disk.sh;
      })
      pkgs.cryptsetup
      pkgs.btrfs-progs
    ];

    environment.etc."braid/config.json".text = builtins.toJSON {
      disks = [
        "/dev/disk/by-id/virtio-disk1"
        "/dev/disk/by-id/virtio-disk2"
        "/dev/disk/by-id/virtio-disk3"
        "/dev/disk/by-id/virtio-disk4"
      ];
      mountPoint = "/mnt/storage";
    };

    # Mount in initrd with degraded option. The x-systemd options prevent the
    # mount from starting until btrfs-device-scan completes. "degraded" allows
    # btrfs to mount with missing members (harmless when all present).
    virtualisation.fileSystems."/mnt/storage" = {
      device = "/dev/mapper/disk1";
      fsType = "btrfs";
      neededForBoot = true;
      options = [
        "degraded"
        "x-systemd.requires=btrfs-device-scan.service"
        "x-systemd.after=btrfs-device-scan.service"
      ];
    };

    # Stage-2 copy of btrfs-device-scan. The x-systemd.requires on the mount
    # persists across switch-root, so the service must also exist in stage 2
    # or systemd will unmount /mnt/storage after switch-root.
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
            # units. Using the real names would make /dev/mapper/disk1 appear,
            # which could trigger dependent units prematurely.
            for disk in ${lib.concatStringsSep " " disks}; do
              echo -n '${passphrase}' | cryptsetup luksOpen --key-file=- "/dev/disk/by-id/virtio-$disk" "$disk-fmt"
            done

            # Create btrfs RAID1 across all drives
            if ! btrfs filesystem show /dev/mapper/disk1-fmt >/dev/null 2>&1; then
              mkfs.btrfs -f -d raid1 -m raid1 ${lib.concatMapStringsSep " " (d: "/dev/mapper/${d}-fmt") disks}
            fi

            # Mount and write test data before bricking disk3
            mkdir -p /tmp/fixture-mount
            mount /dev/mapper/disk1-fmt /tmp/fixture-mount
            echo 'data written before drive death' > /tmp/fixture-mount/survived.txt
            sync
            umount /tmp/fixture-mount

            # Close all — the real cryptsetup units will reopen them
            for disk in ${lib.concatStringsSep " " disks}; do
              cryptsetup luksClose "$disk-fmt"
            done

            # Brick disk3 — zero the LUKS header so cryptsetup fails on it
            dd if=/dev/zero of=/dev/disk/by-id/virtio-disk3 bs=1M count=10
          '';
        };

        # After LUKS devices are unlocked, scan for btrfs devices so the kernel
        # learns the filesystem members at their new paths. Uses "wants" instead
        # of "requires" so disk3's cryptsetup failure doesn't cascade.
        services.btrfs-device-scan = {
          description = "Scan for btrfs multi-device filesystems";
          after = map (d: "systemd-cryptsetup@${d}.service") disks;
          wants = map (d: "systemd-cryptsetup@${d}.service") disks;
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
      # nofail: makes cryptsetup unit Wants instead of Requires of cryptsetup.target
      # x-systemd.device-timeout=10s: don't wait 90s for a missing device
      luks.devices = lib.mkVMOverride (
        lib.genAttrs disks (name: {
          device = "/dev/disk/by-id/virtio-${name}";
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
