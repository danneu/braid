{ config, lib, ... }:
{
  options.btrnas = {
    enable = lib.mkEnableOption "btrnas encrypted storage";

    disks = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [];
      description = "Disk paths (/dev/disk/by-id/...) for the LUKS + btrfs pool.";
    };

    mountPoint = lib.mkOption {
      type = lib.types.str;
      default = "/mnt/storage";
      description = "Where to mount the btrfs pool.";
    };
  };

  config.assertions = [{
    assertion = config.btrnas.enable -> config.btrnas.disks != [];
    message = "btrnas.enable is true but btrnas.disks is empty. Add at least one disk.";
  }];
}
