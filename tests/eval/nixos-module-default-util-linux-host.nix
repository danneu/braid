{
  pkgs,
  self,
  nixpkgs,
}:
let
  # Intent: verifies that nixosModules.default leaves util-linux host-sourced.
  # Why it exists: util-linux is parsed through a stable JSON contract, so
  #   re-pinning it adds closure duplication without parser-stability benefit.
  # Scenario: a consumer overlays util-linux locally while braid's fragile-text
  #   tools still resolve from braid's clean pinned nixpkgs input.
  system = "x86_64-linux";
  cleanPkgs = import self.inputs.nixpkgs { inherit system; };
  hostOverlay = final: prev: {
    util-linux = prev.util-linux.overrideAttrs (old: {
      passthru = (old.passthru or { }) // {
        braidHostMarkerUtilLinux = true;
      };
    });
    cryptsetup = prev.cryptsetup.overrideAttrs (old: {
      passthru = (old.passthru or { }) // {
        braidHostMarkerCryptsetup = true;
      };
    });
  };
  sys = nixpkgs.lib.nixosSystem {
    inherit system;
    modules = [
      self.nixosModules.default
      {
        nixpkgs.overlays = [ hostOverlay ];
        braid.enable = true;
      }
    ];
  };
  utilLinux = sys.config.braid.packages.utilLinux;
  cryptsetup = sys.config.braid.packages.cryptsetup;
in
assert !(cleanPkgs.util-linux.passthru.braidHostMarkerUtilLinux or false);
assert !(cleanPkgs.cryptsetup.passthru.braidHostMarkerCryptsetup or false);
assert (utilLinux.passthru.braidHostMarkerUtilLinux or false);
assert !(cryptsetup.passthru.braidHostMarkerCryptsetup or false);
pkgs.runCommand "eval-nixos-module-util-linux-host" { } ''
  touch $out
''
