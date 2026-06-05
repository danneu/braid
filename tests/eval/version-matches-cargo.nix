{
  pkgs,
  self,
  system,
  cargoToml,
}:
let
  # Intent: enforce the version single-source-of-truth -- the built package's
  #   version must equal cli/Cargo.toml's [package] version.
  # Why it exists: flake.nix#commonArgs reads the version from cli/Cargo.toml via
  #   crateNameFromCargoToml, so this is trivially true today. It fails loudly if
  #   anyone reverts flake.nix to a hardcoded literal that then drifts as
  #   `cargo release` bumps the crate, turning the SoT from a convention into an
  #   enforced invariant. Runs in the release gate and in `nix flake check`.
  # Scenario: a release bumps cli/Cargo.toml to 0.0.2 but a stale hardcoded
  #   flake literal still says 0.0.1 -- the cache would publish a binary whose
  #   `braid --version` disagrees with its release tag.
  #
  # `cargoToml` is passed from the call site (flake.nix), not written here: a
  # literal ./cli/Cargo.toml inside tests/eval/ would resolve to the wrong path.
  pkgVersion = self.packages.${system}.braid-cli-unwrapped.version;
  cargoVersion = (builtins.fromTOML (builtins.readFile cargoToml)).package.version;
in
assert pkgVersion == cargoVersion;
# Pure-eval guard: the assert above does the work; the derivation only needs to
# build (touch $out) once the versions match.
pkgs.runCommand "eval-version-matches-cargo" { } ''
  touch $out
''
