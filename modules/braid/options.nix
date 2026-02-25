{ lib, pkgs, config, ... }:
let
  cfg = config.braid;
  diskNames = builtins.attrNames cfg.disks;
  byIdValues = map (name: cfg.disks.${name}.byId) diskNames;
  inherit (builtins) map length attrValues;
  validDiskKey = name: builtins.match "[a-zA-Z][a-zA-Z0-9_-]*" name != null;
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
      type = lib.types.str;
      default = "/mnt/storage";
      description = "Where to mount the btrfs pool.";
    };

    packages = {
      cryptsetup = lib.mkPackageOption pkgs "cryptsetup" {};
      btrfsProgs = lib.mkPackageOption pkgs "btrfs-progs" {};
      utilLinux = lib.mkPackageOption pkgs "util-linux" {};
      jq = lib.mkPackageOption pkgs "jq" {};
      coreutils = lib.mkPackageOption pkgs "coreutils" {};
    };

    package = lib.mkOption {
      type = lib.types.nullOr lib.types.package;
      default = null;
      description = "The braid CLI package (unwrapped crane output). When set, wraps and installs as 'braid'.";
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = (length diskNames) >= 1;
        message = "braid.disks must contain at least 1 disk when braid.enable = true.";
      }
      {
        assertion = lib.all validDiskKey diskNames;
        message =
          let bad = builtins.filter (n: !validDiskKey n) diskNames;
          in "braid.disks: invalid disk key(s): ${lib.concatStringsSep ", " (map (n: "'${n}'") bad)}. "
             + "Keys must start with a letter and contain only letters, digits, hyphens, or underscores.";
      }
      {
        assertion = lib.all (v: lib.hasPrefix "/dev/disk/by-id/" v) byIdValues;
        message = "All braid.disks.*.byId paths must start with /dev/disk/by-id/.";
      }
      {
        assertion = (length (lib.unique byIdValues)) == (length byIdValues);
        message = "braid.disks contains duplicate byId values. Each disk must have a unique by-id path.";
      }
    ];
  };
}
