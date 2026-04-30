{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.braid;
  braidWrapped = import ./wrapper.nix { inherit cfg pkgs lib; };
  cryptsetup = cfg.packages.cryptsetup;
  btrfsProgs = cfg.packages.btrfsProgs;
  utilLinux = cfg.packages.utilLinux;
  scrubCancelScript = pkgs.writeShellScript "braid-scrub-maybe-cancel" ''
    # If pool is already unmounted during shutdown race, nothing remains to cancel.
    ${utilLinux}/bin/mountpoint -q ${cfg.mountPoint} || exit 0

    # Use the typed scrub-status parser instead of grep -- see
    # cli/src/scrub_cancel.rs. Only cancels if status is Running; clean
    # no-op for terminal non-running states; hard-fails on Unknown so
    # parser drift surfaces instead of silently masking a busy mount.
    # Mount is passed explicitly -- ExecStop has no config-file dependency.
    ${braidWrapped}/bin/braid scrub-cancel --mount ${cfg.mountPoint}
    ret=$?
    # Give the foreground `btrfs scrub start/resume -B` process a chance to
    # rewrite scrub.status.<fsid> with canceled=1 before systemd kills it.
    if [ "$ret" -eq 0 ]; then
      ${pkgs.coreutils}/bin/sleep 2
    fi
    exit "$ret"
  '';
in
{
  config = lib.mkIf cfg.enable {
    # Mount point directory — replaces the old fileSystems entry.
    # Permissions are set by the braid wrapper post-unlock (root:storageGroup 2770).
    systemd.tmpfiles.rules = [
      # State directory — pool config, LUKS header backups, alert flag files.
      # The CLI creates this on first write, but the smartd shell hook needs it
      # to exist before the CLI has ever run.
      "d /var/lib/braid 0750 root root -"
      "d ${cfg.mountPoint} 0755 root root -"
    ]
    ++ lib.optionals cfg.autoUnlock.enable [
      # 0700 root:root — keyfile is sensitive; non-root must not traverse.
      "d /run/braid-key 0700 root root -"
    ];

    # --- Scrub scheduling ---
    # braid-owned scrub timer + service, replacing services.btrfs.autoScrub.
    # Timer lifecycle is tied to braid-online.service: starts when pool comes
    # online, stops when pool goes offline. Persistent=true handles catch-up
    # for overdue scrubs on activation (e.g., pool was locked past a monthly
    # boundary, then unlocked days later).
    systemd.timers.braid-scrub = lib.mkIf cfg.autoScrub.enable {
      description = "Periodic btrfs scrub for braid pool";
      wantedBy = [ "braid-online.service" ];
      bindsTo = [ "braid-online.service" ];
      after = [ "braid-online.service" ];
      timerConfig = {
        OnCalendar = cfg.autoScrub.interval;
        AccuracySec = "1d";
        Persistent = true;
      };
    };

    systemd.services.braid-scrub = lib.mkIf cfg.autoScrub.enable {
      description = "btrfs scrub on ${cfg.mountPoint}";
      documentation = [ "man:btrfs-scrub(8)" ];
      # DefaultDependencies=yes (systemd default) already provides
      # Conflicts=shutdown.target + Before=shutdown.target.
      # Only sleep.target needs explicit declaration.
      conflicts = [ "sleep.target" ];
      before = [ "sleep.target" ];
      bindsTo = [ "braid-online.service" ];
      after = [ "braid-online.service" ];
      unitConfig.ConditionPathIsMountPoint = cfg.mountPoint;
      serviceConfig = {
        # simple (not oneshot) so ExecStop is invoked on stop.
        Type = "simple";
        Nice = 19;
        IOSchedulingClass = "idle";
        # Scheduled/manual scrub: resume saved progress first; start fresh
        # only when btrfs reports nothing resumable.
        ExecStart = "${braidWrapped}/bin/braid scrub-resume-or-start --mount ${cfg.mountPoint}";
        # If the service is stopped before scrub finishes, cancel it.
        ExecStop = scrubCancelScript;
      };
    };

    systemd.services.braid-scrub-resume-trigger = lib.mkIf cfg.autoScrub.enable {
      description = "Pool-online resume trigger for braid scrub";
      documentation = [ "man:btrfs-scrub(8)" ];
      wantedBy = [ "braid-online.service" ];
      bindsTo = [ "braid-online.service" ];
      after = [ "braid-online.service" ];
      conflicts = [ "sleep.target" ];
      before = [ "sleep.target" ];
      unitConfig.ConditionPathIsMountPoint = cfg.mountPoint;
      serviceConfig.Type = "oneshot";
      path = [
        braidWrapped
        pkgs.systemd
      ];
      script = ''
        ret=0
        braid scrub-needs-resume --mount ${cfg.mountPoint} || ret=$?
        case "$ret" in
          0) systemctl start --no-block braid-scrub.service ;;
          1) ;;            # no resume needed -- clean no-op
          *) exit "$ret" ;; # parser/command error -- surface as Result=failed
        esac
      '';
    };

    # Lifecycle owner: "pool is online."
    # ExecStart=/bin/true — the service's purpose is state ownership, not work.
    # ExecStop=braid lock -- unmounts and closes LUKS on shutdown or manual stop.
    # The environment marker lets the wrapper skip its own recursive
    # braid-online stop when reentered from this ExecStop.
    # Only activated by the wrapper on successful unlock/add (mountpoint -q check).
    systemd.services.braid-online = {
      description = "Braid storage pool online";
      # Guard against direct `systemctl start braid-online.service` bypassing
      # the wrapper. When the condition is not met, systemd skips activation
      # (unit stays inactive, systemctl returns 0). The wrapper's own
      # mountpoint -q check (braid-wrapper.sh) is the primary gate; this is
      # defense-in-depth. Out-of-band mount/unmount can leave this stale.
      unitConfig.ConditionPathIsMountPoint = cfg.mountPoint;
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        ExecStart = "${pkgs.coreutils}/bin/true";
        ExecStop = "${pkgs.coreutils}/bin/env BRAID_SYSTEMD_EXECSTOP=1 ${braidWrapped}/bin/braid lock";
        # Raise the stop timeout from the 90s default so a slow braid lock
        # isn't SIGKILL'd mid-operation.
        TimeoutStopSec = "5min";
      };
    };

    # Single orchestrator service that opens all LUKS and mounts pool.
    # Guarantees one passphrase prompt — avoids relying on systemd-ask-password
    # cache sharing behavior.
    # Usage: systemctl start braid-pool.target
    systemd.services.braid-unlock = {
      description = "Open LUKS and mount braid pool";
      serviceConfig = {
        Type = "oneshot";
      };
      unitConfig.ConditionPathIsMountPoint = "!${cfg.mountPoint}";
      path = [
        braidWrapped
        cryptsetup
        btrfsProgs
        utilLinux
      ];
      script = ''
        # --timeout=0: override the 90s default so the passphrase prompt
        # waits indefinitely
        ${pkgs.systemd}/bin/systemd-ask-password \
          --timeout=0 --id=braid "LUKS passphrase for braid pool:" \
        | braid unlock --passphrase-stdin
      '';
    };

    # Start handle: "bring pool online."
    # Wants unlock only — braid-online is activated by the wrapper on success.
    systemd.targets.braid-pool = {
      description = "Braid storage pool online";
      wants = [ "braid-unlock.service" ];
      after = [ "braid-unlock.service" ];
    };

    # --- Auto-unlock via USB keyfile ---

    fileSystems."/run/braid-key" = lib.mkIf cfg.autoUnlock.enable {
      device = cfg.autoUnlock.keyDevice;
      fsType = "auto";
      options = [
        "ro"
        "nosuid"
        "nodev"
        "noexec"
        "nofail" # never fail boot
        "noauto" # only mount on-demand
        "x-systemd.device-timeout=${toString cfg.autoUnlock.timeoutSec}s"
      ];
    };

    systemd.services.braid-auto-unlock = lib.mkIf cfg.autoUnlock.enable {
      description = "Auto-unlock braid pool from USB keyfile";
      wantedBy = [ "multi-user.target" ];
      after = [ "local-fs.target" ];
      unitConfig.ConditionPathIsMountPoint = "!${cfg.mountPoint}";
      # No RemainAfterExit — if USB is absent at boot (service exits 0 on
      # skip), a later `systemctl start braid-auto-unlock` must be able to
      # re-run the service when the USB is inserted. With RemainAfterExit=true,
      # systemd considers the unit "active" after exit 0 and suppresses
      # subsequent starts. See systemd.service(5).
      serviceConfig = {
        Type = "oneshot";
      };
      path = [
        braidWrapped
        cryptsetup
        btrfsProgs
        utilLinux
      ];
      script =
        let
          keyPath = "/run/braid-key/braid.key";
        in
        ''
          # Mount USB via systemd mount unit — this respects the device-timeout
          # configured on the mount unit, so slow USB enumeration gets the full
          # wait window. A direct `mount` call would bypass that timeout.
          # The escaped unit name matches systemd's path encoding for
          # /run/braid-key → run-braid\x2dkey.mount.
          if ! ${pkgs.systemd}/bin/systemctl start run-braid\\x2dkey.mount 2>/dev/null; then
            echo "braid-auto-unlock: USB key device not available, skipping" >&2
            exit 0
          fi

          # Path traversal defense. The keyfile name is a hardcoded literal
          # ("braid.key"), not user-configurable, so CWE-22 via config is
          # eliminated by construction. However, the USB filesystem is
          # attacker-controlled, so we still verify the resolved path stays
          # within /run/braid-key/ to guard against:
          #   - CWE-59: symlinked keyfile (braid.key -> /etc/shadow)
          # realpath -e also fails if the file doesn't exist, so this
          # subsumes the existence check.
          resolved=$(${pkgs.coreutils}/bin/realpath -e "${keyPath}" 2>/dev/null) || {
            echo "braid-auto-unlock: keyfile not found at ${keyPath}, skipping" >&2
            umount /run/braid-key 2>/dev/null || true
            exit 0
          }
          case "$resolved" in
            /run/braid-key/*)
              ;;
            *)
              echo "braid-auto-unlock: keyfile resolves outside mount root ($resolved), refusing" >&2
              umount /run/braid-key 2>/dev/null || true
              exit 0
              ;;
          esac

          # Warn if keyfile is world/group-readable. On vfat (no Unix perms),
          # files are typically 0755 — we can't fix that (vfat doesn't support
          # chmod), so warn rather than fail. The mount point perms (0700) and
          # short mount window limit exposure. Hard-failing here would break
          # the most common USB format.
          perms=$(${pkgs.coreutils}/bin/stat -c '%a' "$resolved" 2>/dev/null || echo "???")
          case "$perms" in
            400|600) ;; # good
            *) echo "braid-auto-unlock: WARNING: keyfile perms are $perms (expected 400)" >&2 ;;
          esac

          if braid unlock --key-file "$resolved"${lib.optionalString cfg.autoUnlock.allowDegraded " --allow-degraded"}; then
            echo "braid-auto-unlock: pool unlocked successfully" >&2
          else
            ret=$?
            if [ $ret -eq 2 ]; then
              echo "braid-auto-unlock: pool has missing devices -- not mounted" >&2
              echo "braid-auto-unlock: set braid.autoUnlock.allowDegraded = true to allow degraded mount" >&2
            else
              echo "braid-auto-unlock: unlock failed (exit $ret), skipping" >&2
            fi
          fi

          # Always unmount USB after use. Never leave keyfile accessible — this
          # is the Unraid CVE pattern (plaintext credential on a mounted FS).
          umount /run/braid-key 2>/dev/null || true
          exit 0
        '';
    };
  };
}
