{ config, lib, pkgs, ... }:
let
  cfg = config.braid;
  braid = pkgs.writeShellApplication {
    name = "braid";
    runtimeInputs = [ pkgs.cryptsetup pkgs.btrfs-progs pkgs.util-linux pkgs.jq pkgs.coreutils ];
    text = builtins.readFile ../../scripts/braid.sh;
  };
in
{
  config = lib.mkIf cfg.enable {
    environment.etc."braid/config.json".text = builtins.toJSON {
      disks = cfg.disks;
      mountPoint = cfg.mountPoint;
    };

    environment.systemPackages = [
      braid
    ];
  };
}
