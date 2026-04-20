{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.braid;

  braid = import ./wrapper.nix { inherit cfg pkgs lib; };

  # Config JSON uses snake_case to match Rust serde field names.
  configFile = (pkgs.formats.json { }).generate "braid-config.json" ({
    mount_point = cfg.mountPoint;
  } // lib.optionalAttrs cfg.fanControl.enable {
    fan_control = {
      pwm = {
        platform_device = cfg.fanControl.pwm.platformDevice;
        number = cfg.fanControl.pwm.number;
        min_start = cfg.fanControl.pwm.minStart;
        max_stop = cfg.fanControl.pwm.maxStop;
      };
      min_temp = cfg.fanControl.minTemp;
      max_temp = cfg.fanControl.maxTemp;
      min_fan_speed_percent = cfg.fanControl.minFanSpeedPercent;
    };
  });
in
{
  config = lib.mkIf cfg.enable {
    environment.etc."braid/config.json".source = configFile;

    environment.systemPackages = lib.optional (cfg.package != null) braid;

    # Shell completion registration (dynamic, via clap_complete CompleteEnv).
    # Each shell sources a small registration script on startup; the actual
    # candidates are computed by calling back into the braid binary on each
    # tab-press, so they always reflect the current pool membership.
    programs.bash.interactiveShellInit = ''
      source <(COMPLETE=bash ${braid}/bin/braid)
    '';
    programs.zsh.interactiveShellInit = ''
      source <(COMPLETE=zsh ${braid}/bin/braid)
    '';
    programs.fish.interactiveShellInit = ''
      COMPLETE=fish ${braid}/bin/braid | source
    '';
  };
}
