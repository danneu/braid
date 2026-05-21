# Test: tool version pinning
#
# What: Validates that runtime tools (btrfs-progs, cryptsetup, util-linux) resolve
# to Nix store paths at the expected versions, and that the braid wrapper
# has correct PATH provenance.
#
# Why: Braid parsers assume specific tool output formats. This test catches version
# drift and PATH leaks where ambient binaries could bypass the pinned toolchain.
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
      };
    };

  testScript = builtins.readFile ./tool-versions.py;
}
