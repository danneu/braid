{ lib, pkgs, config, ... }:
let
  cfg = config.braid;
  diskNames = builtins.attrNames cfg.disks;
  byIdValues = map (name: cfg.disks.${name}.byId) diskNames;
  inherit (builtins) map length attrValues;
  validDiskName = name: builtins.match "[a-zA-Z][a-zA-Z0-9_-]*" name != null && builtins.stringLength name <= 32;
in
{
  options.braid = {
    enable = lib.mkEnableOption "braid encrypted storage";

    disks = lib.mkOption {
      type = lib.types.attrsOf (lib.types.submodule {
        options.byId = lib.mkOption {
          type = lib.types.str;
          description = "Full /dev/disk/by-id/ path for this disk.";
        };
      });
      default = {};
      description = "Named disks for the LUKS + btrfs pool.";
    };

    mountPoint = lib.mkOption {
      type = lib.types.path;
      default = "/mnt/storage";
      description = "Where to mount the btrfs pool.";
    };

    packages = {
      cryptsetup = lib.mkPackageOption pkgs "cryptsetup" {};
      btrfsProgs = lib.mkPackageOption pkgs "btrfs-progs" {};
      utilLinux = lib.mkPackageOption pkgs "util-linux" {};
    };

    package = lib.mkOption {
      type = lib.types.nullOr lib.types.package;
      default = null;
      description = "The braid CLI package (unwrapped crane output). When set, wraps and installs as 'braid'.";
    };

    storageGroup = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = "storage";
      description = "Group for mount point access. Sets root:<group> 2770 on the mount root after mount-producing commands (unlock, add). Set to null to disable.";
    };

    autoUnlock = {
      enable = lib.mkEnableOption "USB keyfile auto-unlock for braid pool";

      # keyDevice must use /dev/disk/by-id/ — /dev/sdX names shift when devices
      # are added or removed. by-id paths use hardware serial numbers and are
      # stable across reboots. See docs/luks-unlock.md § "USB device naming
      # stability".
      keyDevice = lib.mkOption {
        type = lib.types.str;
        default = "";
        description = "Block device for the USB key (/dev/disk/by-id/...).";
      };

      timeoutSec = lib.mkOption {
        type = lib.types.ints.positive;
        default = 5;
        description = "Seconds to wait for USB device before giving up.";
      };

      allowDegraded = lib.mkOption {
        type = lib.types.bool;
        default = false;
        description = "Mount degraded when devices are missing during auto-unlock. New writes will have zero redundancy.";
      };
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = (length diskNames) >= 1;
        message = "braid.disks must contain at least 1 disk when braid.enable = true.";
      }
      {
        assertion = lib.all validDiskName diskNames;
        message =
          let bad = builtins.filter (n: !validDiskName n) diskNames;
          in "braid.disks: invalid disk name(s): ${lib.concatStringsSep ", " (map (n: "'${n}'") bad)}. "
             + "Names must start with a letter, contain only letters, digits, hyphens, or underscores, and be at most 32 characters.";
      }
      {
        assertion = lib.all (v: lib.hasPrefix "/dev/disk/by-id/" v) byIdValues;
        message = "All braid.disks.*.byId paths must start with /dev/disk/by-id/.";
      }
      {
        assertion = (length (lib.unique byIdValues)) == (length byIdValues);
        message = "braid.disks contains duplicate byId values. Each disk must have a unique by-id path.";
      }
      {
        assertion = cfg.package != null;
        message = "braid.package must be set when braid.enable = true. The braid-unlock service requires the CLI binary.";
      }
      {
        assertion = cfg.storageGroup == null || builtins.match "[a-z_][a-z0-9_-]*" cfg.storageGroup != null;
        message = "braid.storageGroup '${toString cfg.storageGroup}' is not a valid Unix group name.";
      }
      {
        assertion = cfg.autoUnlock.enable -> lib.hasPrefix "/dev/disk/by-id/" cfg.autoUnlock.keyDevice;
        message = "braid.autoUnlock.keyDevice must start with /dev/disk/by-id/.";
      }
      {
        assertion = cfg.autoUnlock.enable -> cfg.autoUnlock.timeoutSec > 0;
        message = "braid.autoUnlock.timeoutSec must be positive.";
      }
    ];

    users.groups = lib.mkIf (cfg.storageGroup != null) {
      ${cfg.storageGroup} = {};
    };
  };
}
