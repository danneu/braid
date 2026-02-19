{ config, lib, pkgs, ... }:
let
  cfg = config.btrnas;
  btrnas-add-disk = pkgs.writeShellApplication {
    name = "btrnas-add-disk";
    runtimeInputs = [ pkgs.cryptsetup pkgs.btrfs-progs pkgs.util-linux pkgs.jq ];
    text = builtins.readFile ../../scripts/btrnas-add-disk.sh;
  };
in
{
  config = lib.mkIf cfg.enable {
    environment.etc."btrnas/config.json".text = builtins.toJSON {
      disks = cfg.disks;
      mountPoint = cfg.mountPoint;
    };

    environment.systemPackages = [
      btrnas-add-disk
    ];
  };
}
