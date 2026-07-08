# Intent: verifies that braid.poolBoundServices accepts a service defined by
#   another module.
# Why it exists: the typo guard should require prior service ownership without
#   rejecting legitimate pool consumers.
# Scenario: a deployment defines a local long-running consumer service and asks
#   braid to stamp the pool lifecycle contract onto it.
{
  pkgs,
  linuxPkgs,
  nixpkgs,
  linuxSystem,
}:
let
  good = import ./_braid-eval-harness.nix {
    inherit
      linuxPkgs
      nixpkgs
      linuxSystem
      ;
    poolBoundServices = [ "stub-consumer" ];
    extraModules = [
      {
        systemd.services.stub-consumer = { };
      }
    ];
  };
in
pkgs.runCommand "eval-pool-bound-services-accepts-acknowledged"
  {
    toplevel = good.config.system.build.toplevel;
  }
  ''
    echo ok
    touch $out
  ''
