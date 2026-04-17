# HDD-driven chassis fan control via hddfancontrol.
#
# Defines the systemd service directly instead of using the nixpkgs
# services.hddfancontrol module. The nixpkgs module unconditionally enables
# hddtemp (unnecessary with drivetemp) and injects hddtemp.service
# dependencies that must then be force-overridden. Owning the service avoids
# that brittleness and gives braid full control over the unit lifecycle.
{ config, lib, pkgs, ... }:
let
  cfg = config.braid;
  fc = cfg.fanControl;
  pwmSpec = "${fc.pwmPath}:${toString fc.minStart}:${toString fc.maxStop}";
in
{
  options.braid.fanControl = {
    enable = lib.mkEnableOption "HDD temperature-driven fan control";

    pwmPath = lib.mkOption {
      type = lib.types.str;
      default = "";
      example = "`echo /sys/devices/platform/f71882fg.656/hwmon/hwmon[[:print:]]`/device/pwm2";
      description = ''
        Sysfs path to the chassis fan PWM control file. Shell globs and
        backtick substitution are supported for hwmon numbering (e.g.
        `echo .../hwmon[[:print:]]`). Run `pwmconfig` to find this path.

        Requires a board-specific Super I/O kernel driver (e.g. nct6775,
        f71882fg, it87) loaded in boot.kernelModules. See the fan control
        guide for the full discovery workflow.
      '';
    };

    minStart = lib.mkOption {
      type = lib.types.ints.between 0 255;
      description = ''
        Minimum PWM value to start the fan from standstill. Run
        `hddfancontrol pwm-test -p <pwm-path>` to measure this for your fan.
      '';
    };

    maxStop = lib.mkOption {
      type = lib.types.ints.between 0 255;
      description = ''
        PWM value below which a spinning fan stalls. Run
        `hddfancontrol pwm-test -p <pwm-path>` to measure this for your fan.
      '';
    };

    minTemp = lib.mkOption {
      type = lib.types.ints.between 0 100;
      default = 30;
      description = "Temperature (Celsius) below which the fan runs at minimum speed.";
    };

    maxTemp = lib.mkOption {
      type = lib.types.ints.between 0 100;
      default = 40;
      description = "Temperature (Celsius) above which the fan runs at full speed.";
    };

    minFanSpeedPercent = lib.mkOption {
      type = lib.types.ints.between 0 100;
      default = 20;
      description = ''
        Minimum fan speed as percentage of range. 20 is hddfancontrol's
        upstream conservative default. Setting to 0 allows the fan to stop
        entirely below minTemp -- only safe if the system has other cooling.
      '';
    };

    interval = lib.mkOption {
      type = lib.types.str;
      default = "30s";
      description = "Temperature polling interval (e.g. '30s', '1min').";
    };
  };

  config = lib.mkIf (cfg.enable && fc.enable) {
    assertions = [
      {
        assertion = fc.pwmPath != "";
        message = ''
          braid.fanControl.pwmPath is required. This is the sysfs path to the
          chassis fan's PWM control file (e.g. /sys/devices/platform/.../pwmN).
          Run `pwmconfig` to discover this path. See the fan control guide for
          the full hardware discovery workflow.
        '';
      }
      {
        assertion = fc.maxStop <= fc.minStart;
        message = "braid.fanControl.maxStop (${toString fc.maxStop}) must be <= "
          + "minStart (${toString fc.minStart}). maxStop is the PWM below which a "
          + "spinning fan stalls; minStart is the PWM needed to start from standstill. "
          + "Run `hddfancontrol pwm-test -p <pwm-path>` to measure these values.";
      }
      {
        assertion = fc.minTemp < fc.maxTemp;
        message = "braid.fanControl.minTemp (${toString fc.minTemp}) "
          + "must be less than maxTemp (${toString fc.maxTemp}).";
      }
    ];

    # Expose SATA drive SMART temperatures as hwmon inputs. drivetemp reads
    # via the ATA SCT command, which does not wake sleeping drives (unlike
    # hddtemp's SCSI INQUIRY approach).
    boot.kernelModules = [ "drivetemp" ];

    # --- hddfancontrol daemon ---
    #
    # Defined directly rather than via the nixpkgs services.hddfancontrol
    # module. No hddtemp daemon dependency: hddfancontrol tries drivetemp
    # first in its probe chain (src/probe/mod.rs:84 in 2.0.6), and drivetemp
    # is loaded via boot.kernelModules above.
    #
    # disks = "ata" monitors ALL visible SATA devices, not only braid pool
    # members. Fan control is a chassis safety loop -- drives generate heat
    # regardless of LUKS/btrfs state.
    systemd.services.hddfancontrol-braid = {
      description = "HDD fan control (braid)";
      wantedBy = [ "multi-user.target" ];
      serviceConfig = {
        # Security hardening (matches nixpkgs hddfancontrol module).
        CPUSchedulingPolicy = "rr";
        CPUSchedulingPriority = 49;
        ProtectSystem = "strict";
        PrivateTmp = true;
        ProtectHome = true;
        SystemCallArchitectures = "native";
        MemoryDenyWriteExecute = true;
        NoNewPrivileges = true;
        # Crash recovery: restart on mid-probe drive removal or transient
        # hwmon read errors during hot-swap events.
        Restart = "always";
        RestartSec = 5;
      };
      script = ''
        exec ${lib.getExe pkgs.hddfancontrol} -v INFO daemon \
          -d ata \
          -p ${pwmSpec} \
          --drive-temp-range ${toString fc.minTemp} ${toString fc.maxTemp} \
          --min-fan-speed-prct ${toString fc.minFanSpeedPercent} \
          --interval ${fc.interval} \
          --restore-fan-settings
      '';
    };

    # --- SATA hotswap support ---
    #
    # hddfancontrol resolves drives once at startup and holds that list for
    # the process lifetime. Adding a drive leaves it unmonitored; removing
    # one crashes the daemon on the next probe cycle. The udev rules below
    # restart the daemon on topology changes so the ata selector re-resolves.

    systemd.services.braid-fan-reload = {
      description = "Restart hddfancontrol after SATA drive topology change";
      serviceConfig = {
        Type = "oneshot";
        # Debounce: SATA hotplug produces multiple udev events in quick
        # succession. While this oneshot is active (in the sleep), further
        # start requests are no-ops -- events collapse into one restart.
        ExecStartPre = "${pkgs.coreutils}/bin/sleep 5";
        ExecStart = "${pkgs.systemd}/bin/systemctl restart hddfancontrol-braid.service";
      };
    };

    # Two rules because the systemd/udev integration is asymmetric:
    # - SYSTEMD_WANTS fires on device add (documented device-unit activation)
    # - SYSTEMD_WANTS does NOT fire on device remove; RUN+= is needed instead
    # ID_BUS=="ata" filters out USB mass storage (also appears as /dev/sd*).
    # On remove, ID_BUS persists in the udev database from the earlier add
    # event -- pragmatically reliable on current systemd, with Restart=always
    # as fallback if it ever doesn't match.
    services.udev.extraRules = ''
      ACTION=="add", SUBSYSTEM=="block", KERNEL=="sd*", ENV{DEVTYPE}=="disk", ENV{ID_BUS}=="ata", TAG+="systemd", ENV{SYSTEMD_WANTS}+="braid-fan-reload.service"
      ACTION=="remove", SUBSYSTEM=="block", KERNEL=="sd*", ENV{DEVTYPE}=="disk", ENV{ID_BUS}=="ata", RUN+="${pkgs.systemd}/bin/systemctl start --no-block braid-fan-reload.service"
    '';
  };
}
