{ pkgs, linuxPkgs, nixpkgs, linuxSystem }:
let
  good = import ./_braid-eval-harness.nix {
    inherit linuxPkgs nixpkgs linuxSystem;
    lockSystemdStopDeadlineSecs = 270;
  };
in
pkgs.runCommand "eval-lock-systemd-stop-deadline-ok" {
  inherit (good.config.system.build) toplevel;
} ''
  echo ok
  touch $out
''
