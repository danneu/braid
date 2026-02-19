{
  description = "btrnas — NixOS NAS with LUKS + btrfs RAID1 + Samba";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
  };

  outputs =
    { self, nixpkgs }:
    let
      pkgs = nixpkgs.legacyPackages.aarch64-darwin;
    in
    {
      checks.aarch64-darwin = {
        hello-world = pkgs.testers.nixosTest (import ./tests/hello-world.nix);
      };
    };
}
