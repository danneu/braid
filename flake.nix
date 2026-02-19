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
      packages.aarch64-darwin.playground =
        (pkgs.testers.nixosTest (import ./vm/playground.nix)).driver;

      checks.aarch64-darwin = {
        hello-world = pkgs.testers.nixosTest (import ./tests/0-hello-world.nix);
        luks = pkgs.testers.nixosTest (import ./tests/1-luks.nix);
        btrfs-raid1 = pkgs.testers.nixosTest (import ./tests/2-btrfs-raid1.nix);
        btrfs-heal = pkgs.testers.nixosTest (import ./tests/3-btrfs-heal.nix);
        btrfs-grow = pkgs.testers.nixosTest (import ./tests/3-btrfs-grow.nix);
        btrfs-grow1 = pkgs.testers.nixosTest (import ./tests/3-btrfs-grow1.nix);
        btrfs-shrink = pkgs.testers.nixosTest (import ./tests/3-btrfs-shrink.nix);
        btrfs-degrade = pkgs.testers.nixosTest (import ./tests/3-btrfs-degrade.nix);
        samba = pkgs.testers.nixosTest (import ./tests/4-samba.nix);
        remote-unlock = pkgs.testers.nixosTest (import ./tests/4-remote-unlock.nix);
        degraded-boot = pkgs.testers.nixosTest (import ./tests/4-degraded-boot.nix);
        btrnas-add-disk = pkgs.testers.nixosTest (import ./tests/5-btrnas-add-disk.nix);
      };
    };
}
