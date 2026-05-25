{ pkgs, self, nixpkgs }:
let
  # Intent: verifies that the public flake module supplies braid.package.
  # Why it exists: guide snippets now rely on nixosModules.default rather than
  #   setting braid.package explicitly.
  # Scenario: a minimal `braid.enable = true` NixOS config should evaluate
  #   without forcing users to know the internal unwrapped package attr.
  sys = nixpkgs.lib.nixosSystem {
    system = "x86_64-linux";
    modules = [
      self.nixosModules.default
      { braid.enable = true; }
    ];
  };
in
# Force only config.braid.package, not config.system.build.toplevel or
# config.assertions, so the check stays pure-eval and does not build the real
# braid-cli-unwrapped crane derivation. This guards the flake-level default,
# not the module assertion that fires when a package is absent.
assert sys.config.braid.package != null;
pkgs.runCommand "eval-nixos-module-default-supplies-package" { } ''
  touch $out
''
