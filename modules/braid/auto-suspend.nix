# Auto-suspend: suspends the NAS when idle, wakes for scrub and on-demand via WoL.
#
# Uses autosuspend (Python daemon from nixpkgs) for the idle countdown and
# suspend/wake lifecycle. braid provides `braid idle` as an ExternalCommand
# check for btrfs-specific activity (scrub, balance, replace).
#
# SSH and local-session checks are always on. SMB and NFS checks are
# auto-detected from whether those services are enabled.
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.braid;
  braidWrapped = import ./wrapper.nix { inherit cfg pkgs lib; };
in
{
  options.braid.autoSuspend = {
    enable = lib.mkEnableOption "auto-suspend when NAS is idle";

    idleTime = lib.mkOption {
      type = lib.types.ints.positive;
      default = 900; # 15 minutes
      description = "Seconds of idle time before suspending.";
    };

    wolInterface = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "eno1";
      description = "Network interface to enable Wake-on-LAN on. Required for waking the NAS after suspend.";
    };
  };

  config = lib.mkIf (cfg.enable && cfg.autoSuspend.enable) {
    assertions = [
      {
        assertion = cfg.autoSuspend.wolInterface != null;
        message = "braid.autoSuspend requires Wake-on-LAN to wake the NAS after suspend. "
          + "Set braid.autoSuspend.wolInterface to your primary network interface (e.g. \"eno1\"). "
          + "Find it with: ip link";
      }
      {
        assertion = cfg.autoSuspend.wolInterface == null
          || !(lib.hasPrefix "wl" cfg.autoSuspend.wolInterface);
        message = "braid.autoSuspend.wolInterface is set to \"${cfg.autoSuspend.wolInterface}\" which looks like a WiFi interface. "
          + "Wake-on-LAN requires a wired ethernet interface — the NixOS wakeOnLan option uses ethtool, "
          + "which does not work for WiFi (silently fails). WiFi wake (WoWLAN) is a separate mechanism "
          + "and is unreliable in practice.";
      }
    ];

    # Enable WoL on the specified interface so the NAS can be woken remotely.
    networking.interfaces.${cfg.autoSuspend.wolInterface}.wakeOnLan.enable = true;
    services.autosuspend = {
      enable = true;

      settings = {
        interval = 60;
        idle_time = cfg.autoSuspend.idleTime;
      };

      checks = lib.mkMerge [
        {
          # btrfs exclusive ops (scrub, balance, replace).
          # Fully qualified paths — autosuspend runs this outside braid's wrapper,
          # so PATH is not guaranteed to include coreutils or a shell.
          BraidPool = {
            class = "ExternalCommand";
            command = "${pkgs.coreutils}/bin/timeout 10 ${pkgs.bash}/bin/bash -c '! ${braidWrapped}/bin/braid idle'";
          };
          # SSH sessions always block suspend — braid requires SSH for unlock,
          # and an active session means someone is working on the machine.
          SSH = {
            class = "ActiveConnection";
            ports = "22";
          };
          # Local interactive sessions (TTY, X11, Wayland) block suspend —
          # someone at a keyboard/monitor should not have the machine sleep
          # under them.
          LocalSession = {
            class = "LogindSessionsIdle";
          };
        }
        (lib.mkIf config.services.samba.enable {
          Smb = {
            class = "Smb";
          };
        })
        (lib.mkIf config.services.nfs.server.enable {
          NfsConnections = {
            class = "ActiveConnection";
            ports = "2049";
          };
        })
      ];

      wakeups = {
        BtrfsScrub = {
          class = "SystemdTimer";
          match = "btrfs-scrub@.*";
        };
      };
    };
  };
}
