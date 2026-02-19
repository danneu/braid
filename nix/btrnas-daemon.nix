{ pkgs }:
pkgs.buildGoModule {
  pname = "btrnasd";
  version = "0.1.0";
  src = ../daemon;
  vendorHash = null;
  postInstall = ''
    mv $out/bin/daemon $out/bin/btrnasd
  '';
}
