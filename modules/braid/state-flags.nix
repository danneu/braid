{ pkgs }:
{
  braidTouchFlag = pkgs.writeShellScript "braid-touch-flag" ''
    ${pkgs.coreutils}/bin/touch "$1"
    ${pkgs.coreutils}/bin/chmod 0600 "$1"
  '';
}
