# Test: tool provenance + configured-version alignment
#
# What: Validates that runtime tools (btrfs-progs, cryptsetup,
# util-linux, nut, smartmontools) resolve to /nix/store/ paths via the
# VM's PATH and that each binary's self-reported version matches
# pkgs.<tool>.version from this same evaluation. Separately, validates
# that the braid wrapper can resolve upsc with an empty ambient PATH
# (upsc only -- the other tools have no wrapper-path subtest today).
#
# Why: Catches ambient binaries shadowing the pinned toolchain on the
# VM PATH and package/binary version mismatches (e.g. a patched binary
# whose --version string drifts from pkgs.<tool>.version). Does NOT
# catch nixpkgs version moves -- expected versions read from the same
# pkgs evaluation that builds the VM, so both sides advance together.
# Drift relative to upstream is gated by the manual fixture-refresh
# workflow documented in cli/tests/fixtures/nixos-26.05/README.md and
# docs/design/decisions/010-toolchain-pinning.md.
#
# Dependencies: braid module (options.nix, cli.nix) must wire cfg.packages correctly.
{
  braid-cli-unwrapped,
  braidWrappedPackage,
}:
{
  name = "tool-versions";

  nodes.machine =
    { pkgs, ... }:
    {
      imports = [ ../../modules/braid ];
      braid.enable = true;
      braid.package = braid-cli-unwrapped;

      environment.systemPackages = [
        pkgs.btrfs-progs
        pkgs.cryptsetup
        pkgs.util-linux
        pkgs.nut
        pkgs.smartmontools
        pkgs.jq
        pkgs.coreutils
      ];

      environment.etc."braid/top-level-braid-path".text = "${braidWrappedPackage}/bin/braid\n";

      # Nix-evaluated expected versions — single source of truth
      environment.etc."braid/expected-versions.json".text = builtins.toJSON {
        btrfsProgs = pkgs.btrfs-progs.version;
        cryptsetup = pkgs.cryptsetup.version;
        utilLinux = pkgs.util-linux.version;
        nut = pkgs.nut.version;
        smartmontools = pkgs.smartmontools.version;
      };
    };

  testScript = builtins.readFile ./tool-versions.py;
}
