# Shared braid CLI wrapper used by both storage.nix and cli.nix.
#
# Replaces makeWrapper with a shell script that:
# 1. Puts tool packages on PATH
# 2. Runs the unwrapped braid binary
{
  cfg,
  pkgs,
  lib,
}:
let
  toolPackages =
    (with cfg.packages; [
      cryptsetup
      btrfsProgs
      utilLinux
      nut
      smartmontools
      ethtool
    ])
    ++ [ pkgs.systemd ];
in
pkgs.runCommand "braid" { } ''
  mkdir -p $out/bin
  substitute ${./braid-wrapper.sh} $out/bin/braid \
    --subst-var-by shell '${pkgs.runtimeShell}' \
    --subst-var-by braidBin '${cfg.package}/bin/braid' \
    --subst-var-by toolPath '${lib.makeBinPath toolPackages}'
  chmod +x $out/bin/braid
''
