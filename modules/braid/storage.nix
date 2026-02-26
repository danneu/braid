{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.braid;
  diskKeys = builtins.attrNames cfg.disks;

  # Mapper names are braid-<name> (e.g. braid-toshiba)
  mapperName = name: "braid-${name}";

  # systemd-cryptsetup-generator escapes mapper names for unit instance names
  # (e.g. "braid-toshiba" → "braid\x2dtoshiba"). We must match this escaping
  # when referencing those units in After=, Requires=, etc.
  cryptsetupUnit =
    name: "systemd-cryptsetup@${builtins.replaceStrings [ "-" ] [ "\\x2d" ] (mapperName name)}.service";
in
{
  config = lib.mkIf cfg.enable {
    boot.initrd = {
      supportedFilesystems = [ "btrfs" ];
      systemd.enable = true;

      luks.devices = builtins.listToAttrs (
        map (name: {
          name = mapperName name;
          value = {
            device = cfg.disks.${name}.byId;
            # dm-crypt's internal workqueues add 3-4x queuing overhead regardless
            # of disk type (HDD or SSD). Bypassing them reduces CPU load, latency,
            # and eliminates I/O stall patterns on spinning disks. Requires kernel >= 5.9.
            # TODO: I think this is just doing --perf-no_read_workqueue and --perf-no_write_workqueue, but verify.
            bypassWorkqueues = true;
            crypttabExtraOpts = [
              "nofail"
              "x-systemd.device-timeout=10s"
            ];
          };
        }) diskKeys
      );

      systemd.services.btrfs-device-scan = {
        description = "Scan for btrfs multi-device filesystems";
        after = map cryptsetupUnit diskKeys;
        wants = map cryptsetupUnit diskKeys;
        before = [ "initrd-fs.target" ];
        wantedBy = [ "initrd-fs.target" ];
        unitConfig.DefaultDependencies = false;
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
        };
        script = "${config.braid.packages.btrfsProgs}/bin/btrfs device scan";
      };
    };

    # noauto — not authoritative for mounting. braid-unlock.service (stage-2)
    # and initrd LUKS+mount handle the actual mount. This entry exists so NixOS
    # knows about the mount point for systemctl targets.
    fileSystems.${cfg.mountPoint} = {
      device = "/dev/mapper/${mapperName (builtins.head diskKeys)}";
      fsType = "btrfs";
      neededForBoot = true;
      options = [
        "degraded"
        "nofail"
        "x-systemd.requires=btrfs-device-scan.service"
        "x-systemd.after=btrfs-device-scan.service"
      ];
    };

    services.btrfs.autoScrub = {
      enable = lib.mkDefault true;
      interval = lib.mkDefault "monthly";
      fileSystems = [ cfg.mountPoint ];
    };

    # Stage-2 copy: x-systemd.requires persists across switch-root
    systemd.services.btrfs-device-scan = {
      description = "Scan for btrfs multi-device filesystems";
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
      };
      script = "${config.braid.packages.btrfsProgs}/bin/btrfs device scan";
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
      script = ''
        passphrase=$(${pkgs.systemd}/bin/systemd-ask-password \
          --id=braid "LUKS passphrase for braid pool:")

        opened=""

        ${lib.concatMapStringsSep "\n" (
          name:
          let
            disk = cfg.disks.${name};
          in
          ''
            if [ -e /dev/mapper/${mapperName name} ]; then
              opened="$opened /dev/mapper/${mapperName name}"
            elif [ -e ${disk.byId} ]; then
              if echo "$passphrase" | ${cfg.packages.cryptsetup}/bin/cryptsetup \
                  open --type luks --key-file=- \
                  --perf-no_read_workqueue --perf-no_write_workqueue \
                  ${disk.byId} ${mapperName name} 2>/dev/null; then
                opened="$opened /dev/mapper/${mapperName name}"
              else
                echo "braid-unlock: WARNING: failed to open ${name} — skipping" >&2
              fi
            else
              echo "braid-unlock: WARNING: ${name} not present — skipping" >&2
            fi
          ''
        ) diskKeys}

        if [ -z "$opened" ]; then
          echo "braid-unlock: ERROR: no disks opened, cannot mount pool" >&2
          exit 1
        fi

        ${cfg.packages.btrfsProgs}/bin/btrfs device scan

        first_mapper=$(echo $opened | ${pkgs.coreutils}/bin/cut -d' ' -f1)
        ${cfg.packages.utilLinux}/bin/mount -o degraded "$first_mapper" ${cfg.mountPoint} || {
          ${cfg.packages.utilLinux}/bin/mount "$first_mapper" ${cfg.mountPoint}
        }
      '';
    };

    systemd.targets.braid-pool = {
      description = "Braid storage pool online";
      wants = [ "braid-unlock.service" ];
      after = [ "braid-unlock.service" ];
    };
  };
}
