{ pkgs }:
pkgs.writeShellApplication {
  name = "btrnas-add-disk";
  runtimeInputs = [ pkgs.cryptsetup pkgs.btrfs-progs pkgs.util-linux pkgs.jq ];
  text = builtins.readFile ../scripts/btrnas-add-disk.sh;
}
