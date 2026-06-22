# Auto-suspend: suspends the NAS when idle, wakes for scrub and on-demand via WoL.
#
# Uses autosuspend (Python daemon from nixpkgs) for the idle countdown and
# suspend/wake lifecycle. braid provides `braid idle` as an ExternalCommand
# check for btrfs-specific activity: a running scrub plus any kernel
# exclusive operation (balance, device add, device remove, device replace,
# resize, swap activate). braid also provides `braid wol-ready` as a
# fail-closed check that the configured NIC currently reports Wake-on: g.
# The exclop states are read from /sys/fs/btrfs/<fsid>/exclusive_operation;
# scrub is checked separately because it is not in the kernel exclop set.
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
  grammar = import ./grammar.nix { inherit lib; };
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
        message =
          "braid.autoSuspend requires Wake-on-LAN to wake the NAS after suspend. "
          + "Set braid.autoSuspend.wolInterface to your primary network interface (e.g. \"eno1\"). "
          + "Find it with: ip link";
      }
      {
        assertion =
          cfg.autoSuspend.wolInterface == null || grammar.isValidInterface cfg.autoSuspend.wolInterface;
        message = "braid.autoSuspend.wolInterface must be 1-15 characters of letters, digits, '_', '.', or '-', must not be '.' or '..', and must not start with '-'.";
      }
      {
        assertion =
          cfg.autoSuspend.wolInterface == null || !(lib.hasPrefix "wl" cfg.autoSuspend.wolInterface);
        message =
          "braid.autoSuspend.wolInterface is set to \"${cfg.autoSuspend.wolInterface}\" which looks like a WiFi interface. "
          + "Wake-on-LAN requires a wired ethernet interface -- the NixOS wakeOnLan option uses ethtool, "
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
          # btrfs activity: scrub plus any kernel exclusive operation
          # (balance, device add/remove/replace, resize, swap activate).
          # Fully qualified paths -- autosuspend runs this outside braid's wrapper,
          # so PATH is not guaranteed to include coreutils or a shell.
          BraidPool = {
            class = "ExternalCommand";
            # `timeout` lives inside the bash invocation so its non-zero
            # overrun result is inverted by `!` to 0 -- which autosuspend
            # treats as activity (block suspend), preserving the fail-closed
            # invariant in `docs/design/decisions/016-auto-suspend.md`. `-k 2`
            # escalates TERM to KILL after two more seconds for processes
            # that ignore or delay TERM. An outer `timeout` would fail open:
            # bash gets killed before `!` runs.
            command = "${pkgs.bash}/bin/bash -c '! ${pkgs.coreutils}/bin/timeout -k 2 10 ${braidWrapped}/bin/braid idle'";
          };
          # Block autosuspend-initiated sleep unless the configured NIC reports
          # Wake-on: g. Inverted like BraidPool: `braid wol-ready` exit 0
          # (armed) -> `!` -> 1 -> autosuspend allows suspend; any non-zero
          # (not armed, unverifiable, setup error, or `timeout` overrun) -> `!`
          # -> 0 -> activity -> block suspend. Fail-closed per
          # docs/design/decisions/016-auto-suspend.md.
          BraidWol = {
            class = "ExternalCommand";
            # `timeout` lives inside bash so its overrun result is inverted by
            # `!`, matching the BraidPool timeout invariant.
            command = "${pkgs.bash}/bin/bash -c '! ${pkgs.coreutils}/bin/timeout -k 2 10 ${braidWrapped}/bin/braid wol-ready'";
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
          match = "braid-scrub";
        };
      };
    };
  };
}
