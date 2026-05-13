# Shared initrd fixture for LUKS + btrfs test setup.
#
# NixOS module fragment — import with parameters, e.g.:
#   imports = [ (import ./lib/initrd-fixture.nix { ... }) ];
#
# Sets up an initrd oneshot service that formats empty virtual drives as
# LUKS + btrfs (single or RAID1) before switch-root.
#
# Crypto modules are derived from config.boot.initrd.luks.cryptoModules so
# the algorithm list stays aligned with nixpkgs. dm_mod/dm_crypt and the
# xts->ecb workaround are added here explicitly.
{
  passphrase, # LUKS passphrase
  diskNames, # [ "disk1" "disk2" ... ]
  diskUuidMap ? null, # optional attrset: disk name -> deterministic LUKS UUID
  extraWaitDevices ? [ ], # extra block devices to wait for (e.g., USB)
  extraStorePaths ? [ ], # extra packages in initrd store
  extraPath ? [ ], # extra packages on service PATH
  supportedFilesystems ? [ ], # e.g., [ "btrfs" ] for in-initrd mount
  preCloseScript ? "", # runs after mkfs, before luksClose (mappers open)
  postScript ? "", # runs after luksClose (mappers closed)
  description ? "Prepare LUKS + btrfs fixture",
}:
{
  config,
  pkgs,
  lib,
  ...
}:
let
  luksCfg = config.boot.initrd.luks;

  allWaitDevices = (map (d: "/dev/disk/by-id/virtio-${d}") diskNames) ++ extraWaitDevices;

  mkfsCmd =
    if builtins.length diskNames == 1 then
      "mkfs.btrfs -f -d single -m dup /dev/mapper/braid-${builtins.head diskNames}-fmt"
    else
      "mkfs.btrfs -f -d raid1 -m raid1 "
      + lib.concatMapStringsSep " " (d: "/dev/mapper/braid-${d}-fmt") diskNames;

  uuidCase =
    if diskUuidMap == null then
      ''
        disk1) luks_uuid="11111111-1111-1111-1111-111111111111" ;;
        disk2) luks_uuid="22222222-2222-2222-2222-222222222222" ;;
        disk3) luks_uuid="33333333-3333-3333-3333-333333333333" ;;
        disk4) luks_uuid="44444444-4444-4444-4444-444444444444" ;;
        disk5) luks_uuid="55555555-5555-5555-5555-555555555555" ;;
        *) echo "no deterministic test LUKS UUID for $disk" >&2; exit 1 ;;
      ''
    else
      lib.concatMapStrings (disk: ''
        ${disk}) luks_uuid="${diskUuidMap.${disk}}" ;;
      '') diskNames
      + ''
        *) echo "no deterministic test LUKS UUID for $disk" >&2; exit 1 ;;
      '';
in
{
  boot.initrd = {
    availableKernelModules = [
      "dm_mod"
      "dm_crypt"
    ]
    ++ luksCfg.cryptoModules
    ++ (lib.optional (builtins.elem "xts" luksCfg.cryptoModules) "ecb");
    kernelModules = [ "dm_crypt" ];
    inherit supportedFilesystems;

    systemd.enable = true;
    systemd = {
      storePaths = [
        pkgs.cryptsetup
        pkgs.btrfs-progs
        pkgs.util-linux
      ]
      ++ extraStorePaths;

      services.prepare-luks-btrfs-fixture = {
        inherit description;
        wantedBy = [ "initrd.target" ];
        before = [ "initrd.target" ];
        after = [
          "systemd-modules-load.service"
          "systemd-udevd.service"
        ];
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
        ]
        ++ extraPath;
        script = ''
          set -eu

          # Wait for all devices
          for dev in ${lib.concatStringsSep " " allWaitDevices}; do
            i=0
            while [ "$i" -lt 100 ]; do
              [ -b "$dev" ] && break
              sleep 0.1
              i=$((i + 1))
            done
            test -b "$dev"
          done

          # LUKS format
          for disk in ${lib.concatStringsSep " " diskNames}; do
            dev="/dev/disk/by-id/virtio-$disk"
            if ! cryptsetup isLuks "$dev" 2>/dev/null; then
              case "$disk" in
          ${uuidCase}
              esac
              echo -n '${passphrase}' | cryptsetup luksFormat --batch-mode \
                --uuid "$luks_uuid" \
                --label "braid-$disk" \
                --key-file=- --pbkdf pbkdf2 --pbkdf-force-iterations 1000 "$dev"
            fi
          done

          # LUKS open with -fmt suffix to avoid triggering systemd units
          for disk in ${lib.concatStringsSep " " diskNames}; do
            echo -n '${passphrase}' | cryptsetup luksOpen --key-file=- \
              "/dev/disk/by-id/virtio-$disk" "braid-$disk-fmt"
          done

          # Create btrfs filesystem
          if ! btrfs filesystem show /dev/mapper/braid-${builtins.head diskNames}-fmt >/dev/null 2>&1; then
            ${mkfsCmd}
          fi

          ${preCloseScript}

          # Close all LUKS mappers
          for disk in ${lib.concatStringsSep " " diskNames}; do
            cryptsetup luksClose "braid-$disk-fmt"
          done

          ${postScript}
        '';
      };
    };
  };
}
