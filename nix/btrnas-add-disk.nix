{ pkgs }:
pkgs.writeShellApplication {
  name = "btrnas-add-disk";
  runtimeInputs = [ pkgs.cryptsetup pkgs.btrfs-progs pkgs.util-linux ];
  text = builtins.readFile ../scripts/btrnas-add-disk.sh;
}
