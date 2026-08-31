{
  lib,
  pkgs,
  config,
  ...
}:
let
  cfg = config.braid;
  inherit (import ./constants.nix) braidOnlineStopTimeoutSecs;
  grammar = import ./grammar.nix { inherit lib; };
in
{
  # Traps, not silent removals: both options named a systemd time span, and a
  # config that still sets one is asking for a schedule braid no longer has.
  # Evaluating on with the setting quietly ignored would leave an operator
  # believing they had retimed their scrubs.
  imports = [
    (lib.mkRemovedOptionModule [ "braid" "autoScrub" "interval" ] ''
      braid.autoScrub.interval is gone: scrubs are no longer scheduled on a
      calendar. Set braid.autoScrub.intervalDays instead -- an integer number
      of days measured from the last scrub btrfs recorded, hand-run scrubs
      included. A time-of-day expression has no replacement: the scrub already
      runs at Nice=19 with idle I/O scheduling, which is what an off-peak
      window was buying you.
    '')
    (lib.mkRemovedOptionModule [ "braid" "autoScrub" "retryInterval" ] ''
      braid.autoScrub.retryInterval is gone: a scrub skipped because braid was
      busy with the pool is retried by the next hourly poll, so there is no
      retry interval to configure.
    '')
  ];

  options.braid = {
    enable = lib.mkEnableOption "braid encrypted storage";

    mountPoint = lib.mkOption {
      type = lib.types.path;
      default = "/mnt/storage";
      description = "Canonical absolute path where braid mounts the btrfs pool. Path segments may contain letters, digits, '_', '.', and '-' only; no empty, '.', '..', whitespace, or shell metacharacter segments.";
    };

    packages = {
      cryptsetup = lib.mkPackageOption pkgs "cryptsetup" { };
      btrfsProgs = lib.mkPackageOption pkgs "btrfs-progs" { };
      utilLinux = lib.mkPackageOption pkgs "util-linux" { };
      nut = lib.mkPackageOption pkgs "nut" { };
      smartmontools = lib.mkPackageOption pkgs "smartmontools" { };
      ethtool = lib.mkPackageOption pkgs "ethtool" { };
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

      intervalDays = lib.mkOption {
        type = lib.types.ints.positive;
        default = 30;
        description = ''
          How many days a recorded scrub keeps the pool fresh.

          This is a freshness window, not a calendar schedule: it is measured
          from the last scrub btrfs itself recorded, so a scrub you ran by hand
          counts and pushes the next automatic scrub out by this many days. The
          timer only polls; a poll on a pool scrubbed inside the window exits
          without touching anything.
        '';
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
        assertion = grammar.mountPointOk cfg.mountPoint;
        message = "braid.mountPoint must be a canonical absolute path: segments of letters, digits, '_', '.', '-' separated by single '/', with no empty/'.'/'..' segments, spaces, newlines, or shell metacharacters. Got: '${toString cfg.mountPoint}'.";
      }
      {
        assertion =
          cfg.poolAccessGroup == null || builtins.match "[a-z_][a-z0-9_-]*" cfg.poolAccessGroup != null;
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
    ];

    # A warning, not an assertion: running your own monitoring while braid does
    # the scrub is unusual but legitimate, so it must still evaluate. With the
    # monitor off there is no braid-scrub-failed.service and no device-stats
    # poll, so neither a failed scrub nor scrub-discovered corruption raises any
    # alert.
    warnings =
      lib.optional (cfg.autoScrub.enable && !cfg.monitor.enable) ''
        braid: autoScrub is enabled but monitor is disabled -- scrub failures and
        scrub-discovered corruption will not raise any alert (no beep, no `braid status`
        cause). Enable braid.monitor to alert on scrub problems.
      ''
      # Warnings, not assertions: both ends are legitimate for someone (a tiny
      # SSD pool, an archive that is powered on twice a year), but both are far
      # more often a typo, so they must be visible without being fatal.
      ++ lib.optional (cfg.autoScrub.enable && cfg.autoScrub.intervalDays < 7) ''
        braid: autoScrub.intervalDays = ${toString cfg.autoScrub.intervalDays} re-scrubs the
        pool more than weekly. A scrub reads every allocated block, so on spinning disks
        this is real wear and hours of contention for little extra protection (ADR 015).
      ''
      ++ lib.optional (cfg.autoScrub.enable && cfg.autoScrub.intervalDays > 180) ''
        braid: autoScrub.intervalDays = ${toString cfg.autoScrub.intervalDays} leaves more
        than half a year between scrubs. Silent bit rot is only found by scrubbing, and
        the second copy has to survive until then for RAID1 to repair it (ADR 005).
      '';

    users.groups = lib.mkIf (cfg.poolAccessGroup != null) {
      ${cfg.poolAccessGroup} = { };
    };
  };
}
