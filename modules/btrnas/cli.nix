{ config, lib, pkgs, ... }:
let
  cfg = config.btrnas;
  btrnas-add-disk = pkgs.callPackage ../../nix/btrnas-add-disk.nix {};
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
