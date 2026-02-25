# Test: braid-module-remote-unlock
#
# What: Enables the braid module with braid.remoteUnlock on a 3-disk RAID1
# config. The server boots into initrd with SSH exposed. A client VM SSHes in,
# unlocks all 3 LUKS devices, and the server completes boot with /mnt/storage
# mounted as a RAID1 pool.
#
# Why: Validates that the braid.remoteUnlock module option produces the correct
# initrd SSH config — network, SSH server, root shell, and cryptsetup binary.
# The hand-wired test (4-remote-unlock.nix) proves the mechanism works; this
# test proves the module generates it correctly.
#
# Dependencies: braid-module-raid1 (module storage path works),
# remote-unlock (mechanism works), hello-world (VM infra).
{ braid }:
{ lib, pkgs, ... }:
let
  passphrase = "testpassphrase";
  initrdSshFixtureDir = pkgs.path + "/nixos/tests/initrd-network-ssh";
  diskNames = [ "disk1" "disk2" "disk3" ];
  mapperNames = map (d: "braid-${d}") diskNames;

  cryptsetupUnit = name:
    "systemd-cryptsetup@${builtins.replaceStrings ["-"] ["\\x2d"] name}.service";
in
{
  name = "braid-module-remote-unlock";

  nodes.server = { config, pkgs, ... }: {
    imports = [ ../../modules/braid ];

    braid = {
      enable = true;
      package = braid;
      disks = lib.genAttrs diskNames (d: { byId = "/dev/disk/by-id/virtio-${d}"; });
      remoteUnlock = {
        enable = true;
        authorizedKeys = [ (builtins.readFile (initrdSshFixtureDir + "/id_ed25519.pub")) ];
        hostKeys = [ (initrdSshFixtureDir + "/ssh_host_ed25519_key") ];
      };
    };

    virtualisation.emptyDiskImages = [
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk2"; }
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk3"; }
    ];
    virtualisation.memorySize = 2048;

    environment.systemPackages = [ pkgs.btrfs-progs ];

    boot.kernelParams = [
      "ip=${config.networking.primaryIPAddress}:::255.255.255.0::eth1:none"
    ];

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
      systemd.storePaths = [
        pkgs.cryptsetup
        pkgs.btrfs-progs
        pkgs.util-linux
      ];

      # Fixture: format empty drives as LUKS + btrfs RAID1 before the real
      # cryptsetup units run. Uses -fmt suffix mapper names to avoid triggering
      # systemd device/mount units prematurely.
      systemd.services.prepare-luks-btrfs-fixture = {
        description = "Prepare LUKS + btrfs RAID1 fixture";
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

          for disk in ${lib.concatStringsSep " " diskNames}; do
            echo -n '${passphrase}' | cryptsetup luksOpen --key-file=- \
              "/dev/disk/by-id/virtio-$disk" "braid-$disk-fmt"
          done

          if ! btrfs filesystem show /dev/mapper/braid-disk1-fmt >/dev/null 2>&1; then
            mkfs.btrfs -f -d raid1 -m raid1 \
              ${lib.concatMapStringsSep " " (d: "/dev/mapper/braid-${d}-fmt") diskNames}
          fi

          for disk in ${lib.concatStringsSep " " diskNames}; do
            cryptsetup luksClose "braid-$disk-fmt"
          done
        '';
      };

      # No keyFile — drives must be unlocked over SSH.
      luks.devices = lib.mkVMOverride (
        lib.genAttrs mapperNames (name: {
          device = "/dev/disk/by-id/virtio-${lib.removePrefix "braid-" name}";
          crypttabExtraOpts = [ "nofail" "x-systemd.device-timeout=10s" ];
        })
      );
    };
  };

  nodes.client = { ... }: {
    environment.etc."ssh/test_id_ed25519" = {
      source = initrdSshFixtureDir + "/id_ed25519";
      mode = "0600";
    };
  };

  testScript = builtins.readFile ./06-remote-unlock.py;
}
