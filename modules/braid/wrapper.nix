# Shared braid CLI wrapper used by both storage.nix and cli.nix.
#
# Replaces makeWrapper with a shell script that:
# 1. Puts tool packages on PATH
# 2. Runs the unwrapped braid binary
# 3. On success of mount-producing commands (unlock, add), sets
#    root:<storageGroup> 2770 on the mount point
{ cfg, pkgs, lib }:
let
  toolPackages = with cfg.packages; [ cryptsetup btrfsProgs utilLinux jq coreutils ] ++ [ pkgs.systemd ];
in
pkgs.runCommand "braid" {} ''
  mkdir -p $out/bin
  substitute ${./braid-wrapper.sh} $out/bin/braid \
    --subst-var-by shell '${pkgs.runtimeShell}' \
    --subst-var-by braidBin '${cfg.package}/bin/braid' \
    --subst-var-by toolPath '${lib.makeBinPath toolPackages}' \
    --subst-var-by storageGroup '${if cfg.storageGroup != null then cfg.storageGroup else ""}' \
    --subst-var-by mountpointBin '${cfg.packages.utilLinux}/bin/mountpoint' \
    --subst-var-by chownBin '${cfg.packages.coreutils}/bin/chown' \
    --subst-var-by chmodBin '${cfg.packages.coreutils}/bin/chmod' \
    --subst-var-by mountPointPath '${cfg.mountPoint}'
  chmod +x $out/bin/braid
''
