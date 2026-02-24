{ config, lib, pkgs, ... }:
let
  cfg = config.braid;
  toolPackages = with cfg.packages; [ cryptsetup btrfsProgs utilLinux jq coreutils ];

  braid = pkgs.runCommand "braid-module" {
    nativeBuildInputs = [ pkgs.makeWrapper ];
  } ''
    mkdir -p $out/bin
    makeWrapper ${cfg.package}/bin/braid $out/bin/braid \
      --prefix PATH : ${lib.makeBinPath toolPackages}
  '';
in
{
  config = lib.mkIf cfg.enable {
    assertions = [{
      assertion = cfg.package != null;
      message = "braid.package must be set when braid.enable is true";
    }];

    environment.etc."braid/config.json".text = builtins.toJSON {
      disks = cfg.disks;
      mountPoint = cfg.mountPoint;
    };

    environment.systemPackages = lib.optional (cfg.package != null) braid;

    # Shell completion registration (dynamic, via clap_complete CompleteEnv).
    # Each shell sources a small registration script on startup; the actual
    # candidates are computed by calling back into the braid binary on each
    # tab-press, so they always reflect the current config.
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
