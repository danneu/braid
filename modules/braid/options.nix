{ lib, pkgs, ... }:
{
  options.braid = {
    enable = lib.mkEnableOption "braid encrypted storage";

    disks = lib.mkOption {
      type = lib.types.nonEmptyListOf lib.types.str;
      description = "Disk paths (/dev/disk/by-id/...) for the LUKS + btrfs pool.";
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

    rustPackage = lib.mkOption {
      type = lib.types.nullOr lib.types.package;
      default = null;
      description = "The braid Rust CLI package (unwrapped crane output). Module wraps with cfg.packages.* PATH.";
    };
  };
}
