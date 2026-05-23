{
  lib,
  pkgs,
  config,
  ...
}:
let
  cfg = config.braid;
  inherit (import ./constants.nix) braidOnlineStopTimeoutSecs;
in
{
  options.braid = {
    enable = lib.mkEnableOption "braid encrypted storage";

    mountPoint = lib.mkOption {
      type = lib.types.path;
      default = "/mnt/storage";
      description = "Where to mount the btrfs pool.";
    };

    packages = {
      cryptsetup = lib.mkPackageOption pkgs "cryptsetup" { };
      btrfsProgs = lib.mkPackageOption pkgs "btrfs-progs" { };
      utilLinux = lib.mkPackageOption pkgs "util-linux" { };
      nut = lib.mkPackageOption pkgs "nut" { };
      smartmontools = lib.mkPackageOption pkgs "smartmontools" { };
    };

    package = lib.mkOption {
      type = lib.types.nullOr lib.types.package;
      default = null;
      description = "The braid CLI package (unwrapped crane output). When set, wraps and installs as 'braid'.";
    };

    poolAccessGroup = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = "storage";
      description = "Unix group granted access to the mount root. Sets root:<group> 2770 on the mount root after mount-producing commands (unlock, add). Set to null to disable.";
    };

    lockSystemdStopDeadlineSecs = lib.mkOption {
      type = lib.types.ints.positive;
      default = 270;
      description = ''
        Seconds to wait for /run/braid-pool.lock during braid-online.service ExecStop.
        Must be strictly less than braid-online.service TimeoutStopSec (${toString braidOnlineStopTimeoutSecs} seconds).
      '';
    };

    autoUnlock = {
      enable = lib.mkEnableOption "USB keyfile auto-unlock for braid pool";

      # keyDevice must use /dev/disk/by-id/ — /dev/sdX names shift when devices
      # are added or removed. by-id paths use hardware serial numbers and are
      # stable across reboots. See docs/internals/luks-unlock.md § "USB device naming
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

    autoScrub = {
      enable = lib.mkEnableOption "periodic btrfs scrub" // {
        default = true;
      };

      interval = lib.mkOption {
        type = lib.types.str;
        default = "monthly";
        description = "systemd calendar expression for periodic scrub scheduling.";
      };
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = cfg.package != null;
        message = "braid.package must be set when braid.enable = true. The braid-unlock service requires the CLI binary.";
      }
      {
        assertion = cfg.poolAccessGroup == null || builtins.match "[a-z_][a-z0-9_-]*" cfg.poolAccessGroup != null;
        message = "braid.poolAccessGroup '${toString cfg.poolAccessGroup}' is not a valid Unix group name.";
      }
      {
        assertion = cfg.lockSystemdStopDeadlineSecs < braidOnlineStopTimeoutSecs;
        message = "braid.lockSystemdStopDeadlineSecs (${toString cfg.lockSystemdStopDeadlineSecs}) must be strictly less than braid-online.service TimeoutStopSec (${toString braidOnlineStopTimeoutSecs}).";
      }
      {
        assertion = cfg.autoUnlock.enable -> lib.hasPrefix "/dev/disk/by-id/" cfg.autoUnlock.keyDevice;
        message = "braid.autoUnlock.keyDevice must start with /dev/disk/by-id/.";
      }
      {
        assertion = !(cfg.autoScrub.enable && config.services.btrfs.autoScrub.enable);
        message = "braid.autoScrub replaces services.btrfs.autoScrub. Disable one to avoid duplicate scrubs.";
      }
      {
        assertion = cfg.autoUnlock.enable -> cfg.autoUnlock.timeoutSec > 0;
        message = "braid.autoUnlock.timeoutSec must be positive.";
      }
    ];

    users.groups = lib.mkIf (cfg.poolAccessGroup != null) {
      ${cfg.poolAccessGroup} = { };
    };
  };
}
