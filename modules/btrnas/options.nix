{ lib, ... }:
{
  options.btrnas = {
    enable = lib.mkEnableOption "btrnas encrypted storage";

    disks = lib.mkOption {
      type = lib.types.nonEmptyListOf lib.types.str;
      description = "Disk paths (/dev/disk/by-id/...) for the LUKS + btrfs pool.";
    };

    mountPoint = lib.mkOption {
      type = lib.types.str;
      default = "/mnt/storage";
      description = "Where to mount the btrfs pool.";
    };
  };
}
