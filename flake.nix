{
  description = "braid — NixOS NAS with LUKS + btrfs RAID1";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
  };

  outputs =
    { self, nixpkgs }:
    let
      forAllSystems = nixpkgs.lib.genAttrs [
        "aarch64-darwin"
        "x86_64-linux"
      ];

      checksFor = system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
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
          braid-add-disk = pkgs.testers.nixosTest (import ./tests/5-braid-add-disk.nix);
          first-boot-single-disk = pkgs.testers.nixosTest (import ./tests/6-first-boot-single-disk.nix);
          replace-failed-disk = pkgs.testers.nixosTest (import ./tests/7-replace-failed-disk.nix);
          daemon-hello-world = pkgs.testers.nixosTest (import ./tests/daemon/00-hello-world.nix);
          braid-module-disabled = pkgs.testers.nixosTest (import ./tests/braid-module/00-disabled.nix);
          braid-module-single-disk = pkgs.testers.nixosTest (import ./tests/braid-module/01-single-disk.nix);
          braid-module-raid1 = pkgs.testers.nixosTest (import ./tests/braid-module/02-raid1.nix);
          braid-module-degraded-raid1 = pkgs.testers.nixosTest (import ./tests/braid-module/03-degraded-raid1.nix);
          braid-module-bad-config = pkgs.testers.nixosTest (import ./tests/braid-module/04-bad-config.nix);
          braid-module-single-disk-dead = pkgs.testers.nixosTest (import ./tests/braid-module/05-single-disk-dead.nix);
          braid-module-remote-unlock = pkgs.testers.nixosTest (import ./tests/braid-module/06-remote-unlock.nix);
        };
    in
    {
      packages.aarch64-darwin.playground =
        (nixpkgs.legacyPackages.aarch64-darwin.testers.nixosTest (import ./vm/playground.nix)).driver;

      checks = forAllSystems checksFor;
    };
}
