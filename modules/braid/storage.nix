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
in
{
  config = lib.mkIf cfg.enable {
    # Mount point directory — replaces the old fileSystems entry.
    # Permissions are set by the braid wrapper post-unlock (root:storageGroup 2770).
    systemd.tmpfiles.rules = [
      "d ${cfg.mountPoint} 0755 root root -"
    ]
    ++ lib.optionals cfg.autoUnlock.enable [
      # 0700 root:root — keyfile is sensitive; non-root must not traverse.
      "d /run/braid-key 0700 root root -"
    ];

    services.btrfs.autoScrub = {
      enable = lib.mkDefault true;
      interval = lib.mkDefault "monthly";
      fileSystems = [ cfg.mountPoint ];
    };

    # Wrapped CLI available on PATH
    environment.systemPackages = [ braidWrapped ];

    # Lifecycle owner: "pool is online."
    # ExecStart=/bin/true — the service's purpose is state ownership, not work.
    # ExecStop=braid lock — unmounts and closes LUKS on shutdown or manual stop.
    # Only activated by the wrapper on successful unlock/add (mountpoint -q check).
    systemd.services.braid-online = {
      description = "Braid storage pool online";
      # Guard against direct `systemctl start braid-online.service` bypassing
      # the wrapper. When the condition is not met, systemd skips activation
      # (unit stays inactive, systemctl returns 0). The wrapper's own
      # mountpoint -q check (braid-wrapper.sh) is the primary gate; this is
      # defense-in-depth for the invariant: braid-online active ⟺ pool mounted.
      unitConfig.ConditionPathIsMountPoint = cfg.mountPoint;
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        ExecStart = "${pkgs.coreutils}/bin/true";
        ExecStop = "${braidWrapped}/bin/braid lock";
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
        RemainAfterExit = true;
      };
      unitConfig.ConditionPathIsMountPoint = "!${cfg.mountPoint}";
      path = [
        braidWrapped
        cryptsetup
        btrfsProgs
        utilLinux
      ];
      script = ''
        ${pkgs.systemd}/bin/systemd-ask-password \
          --id=braid "LUKS passphrase for braid pool:" \
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
              echo "braid-auto-unlock: pool has missing devices — not mounted" >&2
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
