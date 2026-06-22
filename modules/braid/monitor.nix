{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.braid;
  beepEnabled = cfg.monitor.beep;
  inherit (import ./hardening.nix { }) base;
  braidWrapped = import ./wrapper.nix { inherit cfg pkgs lib; };
  wrappedAlertCommand = lib.optionalString (cfg.monitor.alertCommand != null) ''
    ${pkgs.coreutils}/bin/timeout -k 5s ${toString cfg.monitor.alertCommandTimeoutSec}s ${pkgs.runtimeShell} -c ${lib.escapeShellArg cfg.monitor.alertCommand} || true
  '';

  # Canonical privilege-dropped beep wrapper. This is the SINGLE source of
  # truth for the alert beep argv: both the beep service script and the
  # /etc/braid/notifier-config.json file reference this derivation by Nix
  # store path, so they cannot drift. Doctor reads the path from the config
  # file and runs this same wrapper as a subprocess.
  braidBeepProbe = pkgs.writeShellScriptBin "braid-beep-probe" ''
    exec ${pkgs.util-linux}/bin/setpriv \
      --reuid=nobody --regid=beep --groups=beep -- \
      ${pkgs.beep}/bin/beep -f 1000 -l 500
  '';

  smartdAlertScript = pkgs.writeShellScript "braid-smartd-alert" ''
    touch /var/lib/braid/smartd-alert
    ${pkgs.systemd}/bin/systemctl start braid-alert.service 2>/dev/null || true
  '';
in
{
  options.braid.monitor = {
    enable = lib.mkEnableOption "disk health monitoring and alerting" // {
      default = true;
    };

    interval = lib.mkOption {
      type = lib.types.str;
      default = "5min";
      description = "Polling interval for btrfs device stats (systemd time span, e.g. \"5min\", \"30s\").";
    };

    beep = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Emit an audible beep via the PC speaker on disk health alerts.";
    };

    alertCommand = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = ''
        Custom command to run on alert. Runs alongside the beep on a Critical
        alert (disk health), and on its own with no beep for a Warning-only
        alert such as a proactive ENOSPC capacity risk.
      '';
    };

    alertCommandTimeoutSec = lib.mkOption {
      type = lib.types.ints.positive;
      default = 60;
      description = ''
        Seconds before braid stops a custom alert command. The bound applies
        to both Critical alerts and Warning-only advisory alerts.
      '';
    };
  };

  config = lib.mkIf (cfg.enable && cfg.monitor.enable) {
    # --- PC speaker setup (only when beep is enabled) ---
    # NixOS inherits Ubuntu's kmod blacklist which blocks pcspkr by default.
    # Same overlay pattern nixpkgs uses for i2c_i801.
    nixpkgs.overlays = lib.mkIf beepEnabled [
      (_final: prev: {
        kmod-blacklist-ubuntu = prev.kmod-blacklist-ubuntu.overrideAttrs (old: {
          installPhase = old.installPhase + ''
            sed -i '/^blacklist pcspkr/d' $out/modprobe.conf
          '';
        });
      })
    ];

    boot.kernelModules = lib.mkIf beepEnabled [ "pcspkr" ];

    systemd.services.braid-pcspkr-load = lib.mkIf beepEnabled {
      description = "Load pcspkr for braid audible alerts";
      wantedBy = [ "multi-user.target" ];
      serviceConfig = base // {
        Type = "oneshot";
        ProtectKernelModules = false;
        CapabilityBoundingSet = [ "CAP_SYS_MODULE" ];
        PrivateNetwork = true;
        ExecStart = "${pkgs.kmod}/bin/modprobe pcspkr";
      };
    };

    # beep refuses to run as root — grant the beep group write access to
    # the PC Speaker evdev device so the alert service can beep unprivileged.
    users.groups.beep = lib.mkIf beepEnabled { };

    services.udev.extraRules = lib.mkIf beepEnabled ''
      ACTION=="add", SUBSYSTEM=="input", ATTRS{name}=="PC Speaker", ENV{DEVNAME}!="", GROUP="beep", MODE="0620"
    '';

    # --- Notifier config (consumed by `braid doctor`) ---
    # Explicit, braid-owned contract: doctor reads this file to discover
    # the canonical beep wrapper path. Doctor never inspects rendered
    # systemd unit text, so a refactor of the alert service script cannot
    # silently break the speaker probe.
    environment.etc."braid/notifier-config.json".text = builtins.toJSON {
      beep_probe_path = if beepEnabled then "${braidBeepProbe}/bin/braid-beep-probe" else null;
    };

    # --- Alert service ---
    systemd.services.braid-alert = {
      description = "Braid disk health alert";
      wants = lib.optionals beepEnabled [
        "braid-pcspkr-load.service"
        "braid-beep.service"
      ];
      after = lib.optionals beepEnabled [ "braid-pcspkr-load.service" ];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
      };
      script = ''
        ${wrappedAlertCommand}
      '';
    };

    # --- Audible alert loop ---
    systemd.services.braid-beep = lib.mkIf beepEnabled {
      description = "Braid audible alert beep loop";
      bindsTo = [ "braid-alert.service" ];
      startLimitIntervalSec = 0;
      serviceConfig = base // {
        Type = "simple";
        Restart = "always";
        RestartSec = 5;
        CapabilityBoundingSet = [
          "CAP_SETUID"
          "CAP_SETGID"
        ];
        PrivateNetwork = true;
        ProtectKernelTunables = true;
        ProtectClock = true;
        RestrictAddressFamilies = [ "AF_UNIX" ];
        RestrictRealtime = true;
      };
      script = ''
        # Exponential backoff resets via service stop on `braid ack`.
        delay=5
        max_delay=900
        while true; do
          ${braidBeepProbe}/bin/braid-beep-probe 2>/dev/null || true
          sleep "$delay"
          delay=$((delay * 2))
          if [ "$delay" -gt "$max_delay" ]; then
            delay=$max_delay
          fi
        done
      '';
    };

    # --- Advisory alert service (non-beeping Warning tier) ---
    # Runs only the user alertCommand -- never the beeper or its loop. oneshot +
    # RemainAfterExit makes the repeated 5-minute `systemctl start` cycles a
    # no-op until `braid ack` stops the unit, so alertCommand fires once per
    # episode rather than every cycle (mirrors the no-beep braid-alert.service
    # form). The Critical/exit-1 path (braid-alert.service) is unchanged.
    systemd.services.braid-alert-advisory = {
      description = "Braid capacity-risk advisory (non-beeping)";
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
      };
      script = ''
        ${wrappedAlertCommand}
        exit 0
      '';
    };

    # --- Scrub-failure hook ---
    # Started by braid-scrub.service's onFailure (Change: ADR 018). Modeled on
    # smartdAlertScript: writes the durable scrub-failed flag (for
    # status/latch/ack) AND starts the Critical beeper immediately. No
    # ConditionPathIsMountPoint -- it must run even when the failure is a lost
    # mount. The scrub unit's SuccessExitStatus=3 keeps scrub-found corruption
    # off this path (it alerts via the device-stats poll per ADR 014); only a
    # genuine failed-to-run/complete scrub reaches here.
    systemd.services.braid-scrub-failed = {
      description = "Record and announce a failed braid scrub";
      serviceConfig = base // {
        Type = "oneshot";
        ReadWritePaths = [ "/var/lib/braid" ];
        CapabilityBoundingSet = "";
        RestrictAddressFamilies = [ "AF_UNIX" ];
      };
      script = ''
        touch /var/lib/braid/scrub-failed
        ${pkgs.systemd}/bin/systemctl start braid-alert.service 2>/dev/null || true
      '';
    };

    # --- Monitor service (pure detector) ---
    systemd.services.braid-monitor = {
      description = "Poll btrfs device stats for disk errors";
      after = [ "systemd-tmpfiles-setup.service" ];
      unitConfig.ConditionPathIsMountPoint = cfg.mountPoint;
      # statx-based gate (STATX_ATTR_MOUNT_ROOT), independent of the
      # /proc/self/mountinfo parse `braid monitor` fails closed on -- skips
      # only a confirmed-offline pool, never the mounted-but-anomalous beep.
      # Keep it: removal means wasteful 5-min offline runs. See ADR 018.
      serviceConfig = base // {
        Type = "oneshot";
        ReadWritePaths = [
          "/var/lib/braid"
          "/run/braid-pool.lock"
        ];
        CapabilityBoundingSet = [ "CAP_SYS_ADMIN" ];
        RestrictAddressFamilies = [ "AF_UNIX" ];
      };
      path = [
        braidWrapped
        cfg.packages.btrfsProgs
      ];
      script = ''
        rc=0
        braid monitor || rc=$?
        if [ "$rc" -eq 1 ]; then
          ${pkgs.systemd}/bin/systemctl start braid-alert.service 2>/dev/null || true
        elif [ "$rc" -eq 3 ]; then
          # Warning-only (e.g. ENOSPC risk): notify via alertCommand, no beep.
          # Must precede the `-ge 2` failure branch so 3 is not misread as a
          # monitor failure. See ADR 018 for the exit-code table.
          ${pkgs.systemd}/bin/systemctl start braid-alert-advisory.service 2>/dev/null || true
        elif [ "$rc" -ge 2 ]; then
          echo "braid monitor failed (exit $rc)" >&2
        fi
        exit 0
      '';
    };

    # --- Monitor timer ---
    systemd.timers.braid-monitor = {
      description = "Periodic braid disk health check";
      wantedBy = [ "timers.target" ];
      timerConfig = {
        OnActiveSec = cfg.monitor.interval;
        OnUnitActiveSec = cfg.monitor.interval;
      };
    };

    # --- smartd integration ---
    services.smartd = {
      enable = lib.mkDefault true;
      defaults.monitored = lib.mkDefault "-a -o on -S on -m <nomailer> -M exec ${smartdAlertScript}";
      # Suppress the NixOS smartd module's own notification handlers.
      # Without these, installing an MTA (e.g. postfix) would cause the
      # module to prepend a second -m/-M exec pair to every config line.
      notifications.mail.enable = lib.mkDefault false;
      notifications.wall.enable = lib.mkDefault false;
    };
  };
}
