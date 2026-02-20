{ config, lib, pkgs, ... }:
let
  cfg = config.braid;
  braid-add-disk = pkgs.writeShellApplication {
    name = "braid-add-disk";
    runtimeInputs = [ pkgs.cryptsetup pkgs.btrfs-progs pkgs.util-linux pkgs.jq ];
    text = builtins.readFile ../../scripts/braid-add-disk.sh;
  };
  braid-remove-disk = pkgs.writeShellApplication {
    name = "braid-remove-disk";
    runtimeInputs = [ pkgs.cryptsetup pkgs.btrfs-progs pkgs.util-linux pkgs.jq ];
    text = builtins.readFile ../../scripts/braid-remove-disk.sh;
  };
  braid-status = pkgs.writeShellApplication {
    name = "braid-status";
    runtimeInputs = [ pkgs.cryptsetup pkgs.btrfs-progs pkgs.util-linux pkgs.jq ];
    text = builtins.readFile ../../scripts/braid-status.sh;
  };
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
      braid-add-disk
      braid-remove-disk
      braid-status
      braid
    ];
  };
}
