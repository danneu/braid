# Test: remote-unlock
#
# What: Server boots into initrd with 3 LUKS-encrypted drives pre-formatted
# as btrfs RAID1. Initrd exposes SSH on :2222 and blocks waiting for LUKS
# passphrases. Client SSHs in, unlocks all 3 drives, and the server continues
# to full boot with the btrfs pool mounted.
#
# Why: This is the boot-time experience. The NAS powers on, you SSH in from
# your laptop, type the passphrase, and the NAS comes up. Without this, you'd
# need a keyboard and monitor attached to unlock.
#
# Dependencies: btrfs-raid1 (LUKS + btrfs RAID1 work), hello-world (VM infra).
#
# Pattern reference:
# - nixpkgs/nixos/tests/systemd-initrd-networkd-ssh.nix
# - nixpkgs/nixos/tests/systemd-initrd-luks-password.nix
#
# Design note: The btrfs pool is NOT mounted in initrd. btrfs RAID1 needs all
# devices present to mount, but LUKS devices are unlocked sequentially via SSH.
# If we used neededForBoot=true, systemd would try to mount as soon as the
# first device appeared and fail. Instead, a stage-2 systemd service runs
# `btrfs device scan` (needed because mkfs recorded the -fmt device paths)
# and mounts the pool after all LUKS devices are available.
{ lib, pkgs, ... }:
let
  passphrase = "testpassphrase";
  initrdSshFixtureDir = pkgs.path + "/nixos/tests/initrd-network-ssh";
  disks = [ "disk1" "disk2" "disk3" ];
in
{
  name = "remote-unlock";

  nodes.server = { config, pkgs, ... }: {
    virtualisation.emptyDiskImages = [
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk2"; }
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk3"; }
    ];
    virtualisation.memorySize = 2048;

    environment.systemPackages = [ pkgs.btrfs-progs ];

    # Stage-2 service: after all LUKS devices are opened (in initrd) and
    # switch-root completes, scan for btrfs devices and mount the pool.
    # This replaces a declarative fileSystems entry because btrfs RAID1
    # can't mount until all member devices are present, and we can't
    # express "wait for all N LUKS devices" in a fileSystems entry.
    systemd.services.mount-btrfs-pool = {
      description = "Scan and mount btrfs RAID1 pool";
      wantedBy = [ "multi-user.target" ];
      after = [ "local-fs.target" ];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
      };
      path = [ pkgs.btrfs-progs pkgs.util-linux ];
      script = ''
        btrfs device scan
        mkdir -p /mnt/storage
        mount /dev/mapper/disk1 /mnt/storage
      '';
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

        # One-shot initrd fixture: format empty drives as LUKS + btrfs RAID1
        # so the cryptsetup units have something to unlock.
        services.prepare-luks-btrfs-fixture = {
          description = "Prepare LUKS + btrfs RAID1 fixture for remote unlock test";
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

            # Open with -fmt suffix to avoid triggering systemd device units.
            # Using the real names would make /dev/mapper/disk1 appear,
            # which could trigger dependent units prematurely.
            for disk in ${lib.concatStringsSep " " disks}; do
              echo -n '${passphrase}' | cryptsetup luksOpen --key-file=- "/dev/disk/by-id/virtio-$disk" "$disk-fmt"
            done

            # Create btrfs RAID1 across all drives
            if ! btrfs filesystem show /dev/mapper/disk1-fmt >/dev/null 2>&1; then
              mkfs.btrfs -f -d raid1 -m raid1 ${lib.concatMapStringsSep " " (d: "/dev/mapper/${d}-fmt") disks}
            fi

            # Close all — the real cryptsetup units will reopen them
            for disk in ${lib.concatStringsSep " " disks}; do
              cryptsetup luksClose "$disk-fmt"
            done
          '';
        };
      };

      # qemu-vm sets boot.initrd.luks.devices with mkVMOverride, so we must too.
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

  nodes.client = { ... }: {
    environment.etc."ssh/test_id_ed25519" = {
      source = initrdSshFixtureDir + "/id_ed25519";
      mode = "0600";
    };
  };

  testScript = builtins.readFile ./remote-unlock.py;
}
