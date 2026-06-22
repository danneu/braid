# HDD-driven chassis fan control via hddfancontrol.
#
# Defines the systemd service directly instead of using the nixpkgs
# services.hddfancontrol module. The nixpkgs module unconditionally enables
# hddtemp (unnecessary with drivetemp) and injects hddtemp.service
# dependencies that must then be force-overridden. Owning the service avoids
# that brittleness and gives braid full control over the unit lifecycle.
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.braid;
  fc = cfg.fanControl;
  inherit (import ./hardening.nix { }) base;
in
{
  options.braid.fanControl = {
    enable = lib.mkEnableOption "HDD temperature-driven fan control";

    pwm = {
      platformDevice = lib.mkOption {
        type = lib.types.str;
        default = "";
        example = "f71882fg.656";
        description = ''
          Platform device name of the Super I/O chip driving the chassis fan,
          as shown in /sys/devices/platform/ (e.g. "nct6775", "f71882fg.656",
          "it87.2608"). Identified from the pwmconfig-surfaced sysfs path:

            pwm=/sys/class/hwmon/hwmonN/device/pwmN  # from pwmconfig
            pwm_dir=$(dirname "$pwm")
            if [ "$(basename "$pwm_dir")" != device ]; then
              pwm_dir="$pwm_dir/device"
            fi
            basename "$(readlink -f "$pwm_dir")"

          The `if` branch handles both sysfs layouts: hwmon*/device/pwmN
          (common on f71882fg, nct6775) and hwmon*/pwmN (fallback). Without
          it, the fallback layout resolves to hwmonN instead of the platform
          device.

          This name is stable across reboots; the hwmonN number is not.
        '';
      };

      number = lib.mkOption {
        type = lib.types.ints.positive;
        example = 2;
        description = ''
          PWM channel number within the platform device (1-based; matches
          pwmN in sysfs). Identified via `pwmconfig`. No default --
          pwm1 is frequently the CPU fan or an unpopulated header.
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
        assertion = fc.pwm.platformDevice != "";
        message =
          "braid.fanControl.pwm.platformDevice is required. "
          + "See the fan control guide for the discovery workflow.";
      }
      {
        assertion = builtins.match "[A-Za-z0-9_.-]+" fc.pwm.platformDevice != null;
        message =
          "braid.fanControl.pwm.platformDevice (${fc.pwm.platformDevice}) "
          + "must be a platform device identifier (e.g. \"f71882fg.656\"), "
          + "not a full path or shell expression. "
          + "Expected characters: A-Z a-z 0-9 _ . -";
      }
      {
        assertion = fc.pwm.maxStop <= fc.pwm.minStart;
        message =
          "braid.fanControl.pwm.maxStop (${toString fc.pwm.maxStop}) must be <= "
          + "pwm.minStart (${toString fc.pwm.minStart}). maxStop is the PWM below which a "
          + "spinning fan stalls; minStart is the PWM needed to start from standstill. "
          + "Run `hddfancontrol pwm-test -p <pwm-path>` to measure these values.";
      }
      {
        assertion = fc.minTemp < fc.maxTemp;
        message =
          "braid.fanControl.minTemp (${toString fc.minTemp}) "
          + "must be less than maxTemp (${toString fc.maxTemp}).";
      }
    ];

    warnings = lib.optional (fc.minFanSpeedPercent == 0) ''
      braid.fanControl.minFanSpeedPercent is 0, so hddfancontrol may stop the
      fan entirely below minTemp. Only use this if the chassis has other
      cooling or is designed for passive airflow.
    '';

    # Expose SATA drive SMART temperatures as hwmon inputs. drivetemp reads
    # via the ATA SCT command, which does not wake sleeping drives (unlike
    # hddtemp's SCSI INQUIRY approach).
    boot.kernelModules = [ "drivetemp" ];

    # --- hddfancontrol daemon ---
    #
    # Defined directly rather than via the nixpkgs services.hddfancontrol
    # module. No hddtemp daemon dependency: hddfancontrol tries drivetemp
    # first in its probe chain -- hddfancontrol 2.1.1, src/probe/mod.rs
    # (fn `prober`) lists drivetemp::Method first in the methods array.
    # drivetemp is loaded via boot.kernelModules above.
    #
    # disks = "ata" monitors ALL visible SATA devices, not only braid pool
    # members. Fan control is a chassis safety loop -- drives generate heat
    # regardless of LUKS/btrfs state.
    systemd.services.hddfancontrol-braid = {
      description = "HDD fan control (braid)";
      wantedBy = [ "multi-user.target" ];
      serviceConfig = base // {
        CPUSchedulingPolicy = "rr";
        CPUSchedulingPriority = 49;
        # Crash recovery: restart on mid-probe drive removal or transient
        # hwmon read errors during hot-swap events.
        Restart = "always";
        RestartSec = 5;
      };
      script = ''
        matches=( \
          /sys/devices/platform/${fc.pwm.platformDevice}/hwmon/hwmon*/device/pwm${toString fc.pwm.number} \
          /sys/devices/platform/${fc.pwm.platformDevice}/hwmon/hwmon*/pwm${toString fc.pwm.number} \
        )
        existing=()
        for path in "''${matches[@]}"; do
          [ -e "$path" ] && existing+=("$path")
        done
        if [ "''${#existing[@]}" -ne 1 ]; then
          echo "braid.fanControl: expected exactly one PWM path matching" >&2
          echo "  /sys/devices/platform/${fc.pwm.platformDevice}/hwmon/hwmon*/{device/,}pwm${toString fc.pwm.number}," >&2
          echo "  found ''${#existing[@]}." >&2
          if [ "''${#existing[@]}" -eq 0 ]; then
            echo "Is the kernel module for ${fc.pwm.platformDevice} loaded and bound?" >&2
            echo "Check: ls /sys/devices/platform/ | grep -i ${fc.pwm.platformDevice}" >&2
          else
            echo "Multiple PWM paths resolved; narrow platformDevice or verify board driver binding." >&2
          fi
          exit 1
        fi
        pwm_path="''${existing[0]}"
        exec ${lib.getExe pkgs.hddfancontrol} -v INFO daemon \
          -d ata \
          -p "$pwm_path:${toString fc.pwm.minStart}:${toString fc.pwm.maxStop}" \
          --drive-temp-range ${toString fc.minTemp} ${toString fc.maxTemp} \
          --min-fan-speed-prct ${toString fc.minFanSpeedPercent} \
          --interval ${lib.escapeShellArg fc.interval} \
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
      serviceConfig = base // {
        Type = "oneshot";
        CapabilityBoundingSet = "";
        RestrictAddressFamilies = [ "AF_UNIX" ];
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
