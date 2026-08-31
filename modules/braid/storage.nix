{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.braid;
  inherit (import ./hardening.nix { }) base;
  inherit (import ./constants.nix) braidOnlineStopTimeoutSecs;
  inherit (import ./state-flags.nix { inherit pkgs; }) braidTouchFlag;
  braidWrapped = import ./wrapper.nix { inherit cfg pkgs lib; };
  cryptsetup = cfg.packages.cryptsetup;
  btrfsProgs = cfg.packages.btrfsProgs;
  utilLinux = cfg.packages.utilLinux;
  scrubCancelScript = pkgs.writeShellScript "braid-scrub-maybe-cancel" ''
    # Record cancel intent BEFORE anything else -- before the mountpoint
    # early-exit and before the cancel ioctl -- so the marker is present on
    # every deliberate stop (braid lock, suspend, shutdown, mount-gone race).
    # btrfs exits 1 for both a cancelled scrub and a genuine failure, so
    # scrub-resume-or-start keys off this marker to exit 0 on a deliberate
    # cancel and let onFailure fire only on a real failure. See ADR 018.
    ${braidTouchFlag} /var/lib/braid/scrub-cancel-requested

    # If pool is already unmounted during shutdown race, nothing remains to cancel.
    ${utilLinux}/bin/mountpoint -q ${cfg.mountPoint} || exit 0

    # braid scrub-cancel calls the kernel BTRFS_IOC_SCRUB_CANCEL ioctl
    # directly -- see cli/src/scrub_cancel.rs. The ioctl is the
    # kernel-authoritative path: no userspace status round-trip, no parser
    # dependency, immune to status-command failure and userspace/kernel
    # state divergence. An idle filesystem returns ENOTCONN, which btrfs-progs
    # renders as exit code 2 with "not running" stderr; braid dispatches on
    # the exit code and maps it to a clean exit 0. Only real cancel-ioctl
    # errors propagate. Mount is passed explicitly --
    # ExecStop has no config-file dependency.
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
    # Permissions are set by Rust post-unlock lifecycle fixups (root:poolAccessGroup 2770).
    systemd.tmpfiles.rules = [
      # State directory — pool config, LUKS header backups, alert flag files.
      # The CLI creates this on first write, but the smartd shell hook needs it
      # to exist before the CLI has ever run.
      "d /var/lib/braid 0700 root root -"
      "d ${cfg.mountPoint} 0755 root root -"
      "f /run/braid-pool.lock 0600 root root -"
    ]
    ++ lib.optionals cfg.autoUnlock.enable [
      # Locked parent -- non-root cannot traverse into the mounted USB.
      "d /run/braid-key 0700 root root -"
      # Mount point for the USB. Permissions of this dir itself are
      # irrelevant once the USB is mounted on top, but the parent's 0700
      # blocks all non-root traversal.
      "d /run/braid-key/mnt 0700 root root -"
    ];

    # --- Scrub scheduling ---
    # braid-owned scrub timer + service, replacing services.btrfs.autoScrub.
    # Timer lifecycle is tied to braid-online.service: starts when the pool
    # comes online, stops when it goes offline.
    #
    # The timer is a cheap poll, not a schedule: braid-scrub.service reads
    # btrfs's own scrub record at entry and exits 0 when the last scrub is
    # still fresh, so the only question this timer answers is "how often do we
    # look?" (ADR 035). Hence:
    #   * OnActiveSec=30s pokes shortly after unlock/boot, so an aborted scrub
    #     resumes promptly. Not 0: that would race the tail of `braid unlock`,
    #     which still holds the pool lock.
    #   * No Persistent. The stamp file it maintains is a second "when did we
    #     last scrub" record, and disagreeing records are what this design
    #     deletes; catch-up is OnActiveSec plus the freshness predicate.
    #   * Never WakeSystem. braid schedules no wakeups (ADR 016); a realtime
    #     hourly elapse that passed during suspend fires promptly on resume, so
    #     a woken machine gets a prompt poll anyway.
    systemd.timers.braid-scrub = lib.mkIf cfg.autoScrub.enable {
      description = "Freshness poll for the braid pool btrfs scrub";
      wantedBy = [ "braid-online.service" ];
      bindsTo = [ "braid-online.service" ];
      after = [ "braid-online.service" ];
      timerConfig = {
        OnActiveSec = "30s";
        OnCalendar = "hourly";
        AccuracySec = "1min";
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
      # Alert on a genuinely failed scrub. Gated on monitor.enable so there is
      # no dangling unit reference when braid-scrub-failed.service (and the
      # braid-alert.service it starts) do not exist. A deliberate cancel
      # (lock/suspend/shutdown) exits 0 via the cancel-request marker, and
      # SuccessExitStatus=3 keeps scrub-found corruption off this path, so
      # onFailure fires only on a real failed-to-run/complete scrub. See ADR 018.
      onFailure = lib.mkIf cfg.monitor.enable [ "braid-scrub-failed.service" ];
      unitConfig = {
        ConditionPathIsMountPoint = cfg.mountPoint;
        # The timer polls hourly and every poll starts this unit, so start-rate
        # limiting would eventually turn "poll until the pool is clear" into
        # "give up silently" -- on a pool held by a balance that legitimately
        # runs for days.
        StartLimitIntervalSec = 0;
      };
      serviceConfig = {
        # simple (not oneshot) so ExecStop is invoked on stop.
        Type = "simple";
        Nice = 19;
        IOSchedulingClass = "idle";
        # btrfs exit 3 = uncorrectable errors found, scrub COMPLETED. Declare it
        # a service success so corruption never reaches onFailure -- it alerts
        # via the monitor's device-stats poll (ADR 014), and this also fixes a
        # latent bug where such a scrub silently left the unit `failed`.
        #
        # braid exit 4 = the busy gate skipped this run: braid was already
        # working on the pool, so no scrub started and nothing was touched. A
        # skip is not a failure, so it must stay off onFailure (no beep, no
        # scrub-failed flag). It carries no retry apparatus either -- the next
        # hourly poll IS the retry, which is why RestartForceExitStatus and
        # RestartSec are gone. Genuine failures (exit 1) keep
        # fail-once/alert-once semantics.
        #
        # Exit 0 now also covers "not due" and "a scrub is already running":
        # both mean the pool owes no scrub, so they are successes, not skips.
        SuccessExitStatus = [
          3
          4
        ];
        # Poll entry point: exits 0 without touching the pool when btrfs's own
        # record shows a scrub inside the freshness window; otherwise resumes
        # saved progress, or starts fresh when nothing is resumable. The window
        # is passed on the command line, never read from a config file, so the
        # scrub units stay config-file-free (ADR 018 thin-systemd-layer).
        ExecStart = "${braidWrapped}/bin/braid scrub-resume-or-start --mount ${cfg.mountPoint} --fresh-for-secs ${
          toString (cfg.autoScrub.intervalDays * 86400)
        }";
        # If the service is stopped before scrub finishes, cancel it.
        ExecStop = scrubCancelScript;
      };
    };

    # Lifecycle owner: "pool is online."
    # ExecStart=/bin/true — the service's purpose is state ownership, not work.
    # ExecStop=braid lock --systemd-stop unmounts and closes LUKS on shutdown
    # or manual stop with a deadline below TimeoutStopSec.
    # Only activated by Rust post-success lifecycle fixups on successful
    # unlock/add/recover (mountpoint -q check).
    systemd.services.braid-online = {
      description = "Braid storage pool online";
      # Guard against direct `systemctl start braid-online.service` bypassing
      # the CLI. When the condition is not met, systemd skips activation
      # (unit stays inactive, systemctl returns 0). The CLI's mountpoint
      # check in Rust is the primary gate; this is
      # defense-in-depth. Out-of-band mount/unmount can leave this stale.
      unitConfig.ConditionPathIsMountPoint = cfg.mountPoint;
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        ExecStart = "${pkgs.coreutils}/bin/true";
        ExecStop = "${braidWrapped}/bin/braid lock --systemd-stop --deadline-secs ${toString cfg.lockSystemdStopDeadlineSecs}";
        # Raise the stop timeout from the 90s default so a slow braid lock
        # isn't SIGKILL'd mid-operation.
        TimeoutStopSec = "${toString braidOnlineStopTimeoutSecs}s";
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
    # Wants unlock only -- braid-online is activated by Rust dispatch on success.
    systemd.targets.braid-pool = {
      description = "Braid storage pool online";
      wants = [ "braid-unlock.service" ];
      after = [ "braid-unlock.service" ];
    };

    # Seal the offline pool mountpoint immutable (chattr +i) so a process writing
    # ${cfg.mountPoint} while the pool is unmounted fails loudly with EPERM
    # instead of silently landing data on the root filesystem, which the pool
    # then shadows on mount. The sole automatic seal site. See
    # docs/design/decisions/028-immutable-unmounted-mountpoint.md.
    #
    # Type=oneshot with NO RemainAfterExit: the unit returns to inactive (dead)
    # once ExecStart exits, so NixOS re-runs it on every `nixos-rebuild
    # switch`/`test` as well as every boot (self-healing). The static
    # ${cfg.mountPoint} is created by the tmpfiles rule above and sealed here
    # before any `braid add` runs, so the pool always mounts OVER an
    # already-sealed dir and +i persists underneath.
    #
    # ConditionPathIsMountPoint=! gates the seal to the offline window
    # (belt-and-suspenders alongside the in-CLI STATX_ATTR_MOUNT_ROOT fd check),
    # so a mounted `nixos-rebuild switch` never seals the live pool root.
    #
    # before braid-auto-unlock.service is load-bearing, not a nicety: both units
    # are pulled in by multi-user.target, so without the edge they race. If
    # auto-unlock won, it would mount the pool and this unit's
    # ConditionPathIsMountPoint=! would then skip the seal -- so an
    # auto-unlock-with-USB NAS (which never boots offline) would never seal the
    # bare dir. Ordering before auto-unlock runs the seal in the pre-mount window
    # every boot; the pool then mounts over the sealed dir and persistence
    # carries it. When autoUnlock is disabled the unit does not exist and
    # `before` is a harmless no-op ordering string.
    #
    # The seal is pure syscalls (open/statx/ioctl), so only braidWrapped is
    # needed on PATH -- no cryptsetup/btrfs/util-linux. `script` (not a relative
    # `ExecStart = braid ...`) compiles to an absolute generated-script ExecStart
    # that resolves `braid` through the unit PATH, matching braid-unlock.
    systemd.services.braid-seal-mountpoint = {
      description = "Seal braid pool mountpoint immutable while offline";
      wantedBy = [ "multi-user.target" ];
      after = [
        "local-fs.target"
        "systemd-tmpfiles-setup.service"
      ];
      before = [ "braid-auto-unlock.service" ];
      unitConfig.ConditionPathIsMountPoint = "!${cfg.mountPoint}";
      serviceConfig = base // {
        Type = "oneshot";
        ReadWritePaths = [ (builtins.dirOf cfg.mountPoint) ];
        CapabilityBoundingSet = [ "CAP_LINUX_IMMUTABLE" ];
        PrivateNetwork = true;
        PrivateDevices = true;
      };
      path = [ braidWrapped ];
      script = "braid seal-mountpoint";
    };

    # --- Auto-unlock via USB keyfile ---

    fileSystems."/run/braid-key/mnt" = lib.mkIf cfg.autoUnlock.enable {
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
          keyPath = "/run/braid-key/mnt/braid.key";
        in
        ''
          # Always unmount USB after use. Never leave keyfile accessible -- this
          # is the Unraid CVE pattern (plaintext credential on a mounted FS).
          # Install before mounting so every exit path cleans up.
          trap 'umount /run/braid-key/mnt 2>/dev/null || true' EXIT

          # Mount USB via systemd mount unit — this respects the device-timeout
          # configured on the mount unit, so slow USB enumeration gets the full
          # wait window. A direct `mount` call would bypass that timeout.
          # The escaped unit name matches systemd's path encoding for
          # /run/braid-key/mnt -> run-braid\x2dkey-mnt.mount.
          if ! ${pkgs.systemd}/bin/systemctl start run-braid\\x2dkey-mnt.mount 2>/dev/null; then
            echo "braid-auto-unlock: USB key device not available, skipping" >&2
            exit 0
          fi

          # Path traversal defense. The keyfile name is a hardcoded literal
          # ("braid.key"), not user-configurable, so CWE-22 via config is
          # eliminated by construction. However, the USB filesystem is
          # attacker-controlled, so we still verify the resolved path stays
          # within /run/braid-key/mnt/ to guard against:
          #   - CWE-59: symlinked keyfile (braid.key -> /etc/shadow)
          # realpath -e also fails if the file doesn't exist, so this
          # subsumes the existence check.
          resolved=$(${pkgs.coreutils}/bin/realpath -e "${keyPath}" 2>/dev/null) || {
            echo "braid-auto-unlock: keyfile not found at ${keyPath}, skipping" >&2
            exit 0
          }
          case "$resolved" in
            /run/braid-key/mnt/*)
              ;;
            *)
              echo "braid-auto-unlock: keyfile resolves outside mount root ($resolved), refusing" >&2
              exit 0
              ;;
          esac

          # Warn if keyfile is world/group-readable. On vfat (no Unix perms),
          # files are typically 0755 — we can't fix that (vfat doesn't support
          # chmod), so warn rather than fail. The locked parent directory and
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

          exit 0
        '';
    };
  };
}
