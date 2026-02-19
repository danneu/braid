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
        luks = pkgs.testers.nixosTest (import ./tests/luks.nix);
        btrfs-raid1 = pkgs.testers.nixosTest (import ./tests/btrfs-raid1.nix);
        btrfs-heal = pkgs.testers.nixosTest (import ./tests/btrfs-heal.nix);
        btrfs-grow = pkgs.testers.nixosTest (import ./tests/btrfs-grow.nix);
        btrfs-grow1 = pkgs.testers.nixosTest (import ./tests/btrfs-grow1.nix);
        btrfs-shrink = pkgs.testers.nixosTest (import ./tests/btrfs-shrink.nix);
        btrfs-degrade = pkgs.testers.nixosTest (import ./tests/btrfs-degrade.nix);
        samba = pkgs.testers.nixosTest (import ./tests/samba.nix);
        remote-unlock = pkgs.testers.nixosTest (import ./tests/remote-unlock.nix);
      };
    };
}
