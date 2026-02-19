{ config, lib, pkgs, ... }:
let
  cfg = config.braid;
  diskAttrs = map (d: { name = builtins.baseNameOf d; device = d; }) cfg.disks;
  mapperNames = map (d: d.name) diskAttrs;

  # systemd-cryptsetup-generator escapes mapper names for unit instance names
  # (e.g. "virtio-disk1" → "virtio\x2ddisk1"). We must match this escaping
  # when referencing those units in After=, Requires=, etc.
  cryptsetupUnit = name:
    "systemd-cryptsetup@${builtins.replaceStrings ["-"] ["\\x2d"] name}.service";
in
{
  config = lib.mkIf cfg.enable {
    braid.daemon.enable = lib.mkDefault true;

    boot.initrd = {
      supportedFilesystems = [ "btrfs" ];
      systemd.enable = true;

      luks.devices = builtins.listToAttrs (map (d: {
        name = d.name;
        value = {
          device = d.device;
          crypttabExtraOpts = [ "nofail" "x-systemd.device-timeout=10s" ];
        };
      }) diskAttrs);

      systemd.services.btrfs-device-scan = {
        description = "Scan for btrfs multi-device filesystems";
        after = map cryptsetupUnit mapperNames;
        wants = map cryptsetupUnit mapperNames;
        before = [ "initrd-fs.target" ];
        wantedBy = [ "initrd-fs.target" ];
        unitConfig.DefaultDependencies = false;
        serviceConfig = { Type = "oneshot"; RemainAfterExit = true; };
        script = "${pkgs.btrfs-progs}/bin/btrfs device scan";
      };
    };

    fileSystems.${cfg.mountPoint} = {
      device = "/dev/mapper/${builtins.head mapperNames}";
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
      serviceConfig = { Type = "oneshot"; RemainAfterExit = true; };
      script = "${pkgs.btrfs-progs}/bin/btrfs device scan";
    };
  };
}
