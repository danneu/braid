{
  description = "braid — NixOS NAS with LUKS + btrfs RAID1";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
    crane.url = "github:ipetkov/crane";
  };

  outputs =
    {
      self,
      nixpkgs,
      crane,
    }:
    let
      forAllSystems = nixpkgs.lib.genAttrs [
        "aarch64-darwin"
        "x86_64-linux"
      ];

      craneFor =
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          craneLib = crane.mkLib pkgs;
          # cleanCargoSource strips test fixtures and snapshots — include them
          src = pkgs.lib.cleanSourceWith {
            src = ./cli;
            filter =
              path: type:
              (craneLib.filterCargoSources path type)
              || (builtins.match ".*tests/fixtures/.*" path != null)
              || (builtins.match ".*\\.snap$" path != null);
          };
          commonArgs = {
            inherit src;
            pname = "braid-cli";
            version = "0.1.0";
          };
          cargoArtifacts = craneLib.buildDepsOnly commonArgs;
          braid-cli-unwrapped = craneLib.buildPackage (commonArgs // { inherit cargoArtifacts; });
          toolPath = pkgs.lib.makeBinPath [
            pkgs.cryptsetup
            pkgs.btrfs-progs
            pkgs.util-linux
            pkgs.jq
            pkgs.coreutils
          ];
          braid = pkgs.runCommand "braid" { nativeBuildInputs = [ pkgs.makeWrapper ]; } ''
            mkdir -p $out/bin
            makeWrapper ${braid-cli-unwrapped}/bin/braid $out/bin/braid \
              --prefix PATH : ${toolPath}
          '';
        in
        {
          inherit braid-cli-unwrapped;
          inherit braid;
        };

      packagesFor =
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          isLinux = builtins.match ".*-linux" system != null;
        in
        {
          inherit (craneFor system) braid-cli-unwrapped;
        }
        // (if isLinux then { inherit (craneFor system) braid; } else { })
        // (
          if system == "aarch64-darwin" then
            {
              playground = (pkgs.testers.nixosTest (import ./vm/playground.nix)).driver;
            }
          else
            { }
        );

      checksFor =
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          # VM tests run Linux — build the binary for the right platform
          linuxSystem = builtins.replaceStrings [ "-darwin" ] [ "-linux" ] system;
          linuxCrane = craneFor linuxSystem;
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
          braid-add-disk = pkgs.testers.nixosTest (
            import ./tests/5-braid-add-disk.nix {
              braid = linuxCrane.braid;
            }
          );
          first-boot-single-disk = pkgs.testers.nixosTest (import ./tests/6-first-boot-single-disk.nix);
          replace-failed-disk = pkgs.testers.nixosTest (
            import ./tests/7-replace-failed-disk.nix {
              braid = linuxCrane.braid;
            }
          );
          braid-remove-disk = pkgs.testers.nixosTest (
            import ./tests/9-braid-remove-disk.nix {
              braid = linuxCrane.braid;
            }
          );
          braid-unified = pkgs.testers.nixosTest (
            import ./tests/12-braid-unified.nix {
              braid = linuxCrane.braid;
            }
          );
          braid-bootstrap = pkgs.testers.nixosTest (
            import ./tests/13-braid-bootstrap.nix {
              braid = linuxCrane.braid;
            }
          );
          braid-plan-rust = pkgs.testers.nixosTest (
            import ./tests/15-braid-plan-rust.nix {
              braid = linuxCrane.braid;
            }
          );
          braid-apply-rust = pkgs.testers.nixosTest (
            import ./tests/16-braid-apply-rust.nix {
              braid = linuxCrane.braid;
            }
          );
          braid-status-rust = pkgs.testers.nixosTest (
            import ./tests/18-braid-status-rust.nix {
              braid = linuxCrane.braid;
            }
          );
          braid-init-disk-rust = pkgs.testers.nixosTest (
            import ./tests/19-braid-init-disk-rust.nix {
              braid = linuxCrane.braid;
            }
          );
          luks-header-backup = pkgs.testers.nixosTest (
            import ./tests/21-luks-header-backup.nix {
              braid = linuxCrane.braid;
            }
          );
          tool-versions = pkgs.testers.nixosTest (
            import ./tests/17-tool-versions.nix {
              braid-cli-unwrapped = linuxCrane.braid-cli-unwrapped;
            }
          );
          capture-tool-fixtures = pkgs.testers.nixosTest (import ./tests/capture-tool-fixtures.nix);
          progress-monitoring = pkgs.testers.nixosTest (import ./tests/20-progress-monitoring.nix);
          braid-module-disabled = pkgs.testers.nixosTest (import ./tests/braid-module/00-disabled.nix);
          braid-module-single-disk = pkgs.testers.nixosTest (
            import ./tests/braid-module/01-single-disk.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          braid-module-raid1 = pkgs.testers.nixosTest (
            import ./tests/braid-module/02-raid1.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          braid-module-degraded-raid1 = pkgs.testers.nixosTest (
            import ./tests/braid-module/03-degraded-raid1.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          braid-module-bad-config = pkgs.testers.nixosTest (
            import ./tests/braid-module/04-bad-config.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          braid-module-single-disk-dead = pkgs.testers.nixosTest (
            import ./tests/braid-module/05-single-disk-dead.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          braid-module-remote-unlock = pkgs.testers.nixosTest (
            import ./tests/braid-module/06-remote-unlock.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
        };
    in
    {
      nixosModules.default = import ./modules/braid;

      packages = forAllSystems packagesFor;

      checks = forAllSystems checksFor;

      devShells = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.mkShell {
            buildInputs = with pkgs; [
              cargo
              rustc
              rustfmt
              clippy
            ];
          };
        }
      );
    };
}
