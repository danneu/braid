{ config, lib, pkgs, ... }:
let
  cfg = config.braid;
  braid-add-disk = pkgs.writeShellApplication {
    name = "braid-add-disk";
    runtimeInputs = [ pkgs.cryptsetup pkgs.btrfs-progs pkgs.util-linux pkgs.jq ];
    text = builtins.readFile ../../scripts/braid-add-disk.sh;
  };
in
{
  config = lib.mkIf cfg.enable {
    environment.etc."braid/config.json".text = builtins.toJSON {
      disks = cfg.disks;
      mountPoint = cfg.mountPoint;
    };

    environment.systemPackages = [
      braid-add-disk
    ];
  };
}
