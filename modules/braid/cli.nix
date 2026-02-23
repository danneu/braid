{ config, lib, pkgs, ... }:
let
  cfg = config.braid;
  toolPackages = with cfg.packages; [ cryptsetup btrfsProgs utilLinux jq coreutils ];

  braid = pkgs.writeShellApplication {
    name = "braid";
    runtimeInputs = toolPackages;
    text = builtins.readFile ../../scripts/braid.sh;
  };

  braid-rust-wrapped = pkgs.runCommand "braid-rust-module" {
    nativeBuildInputs = [ pkgs.makeWrapper ];
  } ''
    mkdir -p $out/bin
    makeWrapper ${cfg.rustPackage}/bin/braid $out/bin/braid-rust \
      --prefix PATH : ${lib.makeBinPath toolPackages}
  '';
in
{
  config = lib.mkIf cfg.enable {
    environment.etc."braid/config.json".text = builtins.toJSON {
      disks = cfg.disks;
      mountPoint = cfg.mountPoint;
    };

    environment.systemPackages = [ braid ]
      ++ lib.optional (cfg.rustPackage != null) braid-rust-wrapped;
  };
}
