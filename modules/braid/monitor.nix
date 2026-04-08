{ config, lib, pkgs, ... }:
let
  cfg = config.braid;
  beepEnabled = cfg.monitor.beep;
  braidWrapped = import ./wrapper.nix { inherit cfg pkgs lib; };

  # Canonical privilege-dropped beep wrapper. This is the SINGLE source of
  # truth for the alert tone argv: both the alert service script and the
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
    enable = lib.mkEnableOption "disk health monitoring and alerting" // { default = true; };

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
      description = "Custom command to run on alert, in addition to beep.";
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

    # beep refuses to run as root — grant the beep group write access to
    # the PC Speaker evdev device so the alert service can beep unprivileged.
    users.groups.beep = lib.mkIf beepEnabled {};

    services.udev.extraRules = lib.mkIf beepEnabled ''
      ACTION=="add", SUBSYSTEM=="input", ATTRS{name}=="PC Speaker", ENV{DEVNAME}!="", GROUP="beep", MODE="0620"
    '';

    # --- Notifier config (consumed by `braid doctor`) ---
    # Explicit, braid-owned contract: doctor reads this file to discover
    # the canonical beep wrapper path. Doctor never inspects rendered
    # systemd unit text, so a refactor of the alert service script cannot
    # silently break the speaker probe.
    environment.etc."braid/notifier-config.json".text = builtins.toJSON {
      beep_probe_path =
        if beepEnabled
        then "${braidBeepProbe}/bin/braid-beep-probe"
        else null;
    };

    # --- Alert service ---
    systemd.services.braid-alert = {
      description = "Braid disk health alert (audible beep if enabled)";
      serviceConfig = if beepEnabled then {
        Type = "simple";
      } else {
        Type = "oneshot";
        RemainAfterExit = true;
      };
      script = ''
        ${lib.optionalString beepEnabled ''
          ${pkgs.kmod}/bin/modprobe pcspkr 2>/dev/null || true
        ''}
        ${lib.optionalString (cfg.monitor.alertCommand != null) ''
          ${cfg.monitor.alertCommand} || true
        ''}
        ${lib.optionalString beepEnabled ''
          while true; do
            ${braidBeepProbe}/bin/braid-beep-probe 2>/dev/null || true
            sleep 15
          done
        ''}
      '';
    };

    # --- Monitor service (pure detector) ---
    systemd.services.braid-monitor = {
      description = "Poll btrfs device stats for disk errors";
      unitConfig.ConditionPathIsMountPoint = cfg.mountPoint;
      serviceConfig.Type = "oneshot";
      path = [ braidWrapped cfg.packages.btrfsProgs ];
      script = ''
        rc=0
        braid monitor || rc=$?
        if [ "$rc" -eq 1 ]; then
          ${pkgs.systemd}/bin/systemctl start braid-alert.service 2>/dev/null || true
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
      defaults.monitored = lib.mkDefault
        "-a -o on -S on -m <nomailer> -M exec ${smartdAlertScript}";
      # Suppress the NixOS smartd module's own notification handlers.
      # Without these, installing an MTA (e.g. postfix) would cause the
      # module to prepend a second -m/-M exec pair to every config line.
      notifications.mail.enable = lib.mkDefault false;
      notifications.wall.enable = lib.mkDefault false;
    };
  };
}
