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
  };
}
