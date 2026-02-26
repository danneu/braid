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
            version = "0.0.1";
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
          hello-world = pkgs.testers.nixosTest (import ./tests/hello-world.nix);
          luks = pkgs.testers.nixosTest (import ./tests/storage/luks.nix);
          btrfs-raid1 = pkgs.testers.nixosTest (import ./tests/storage/btrfs-raid1.nix);
          btrfs-heal = pkgs.testers.nixosTest (import ./tests/storage/btrfs-heal.nix);
          btrfs-grow = pkgs.testers.nixosTest (import ./tests/storage/btrfs-grow.nix);
          btrfs-grow1 = pkgs.testers.nixosTest (import ./tests/storage/btrfs-grow1.nix);
          btrfs-shrink = pkgs.testers.nixosTest (import ./tests/storage/btrfs-shrink.nix);
          btrfs-degrade = pkgs.testers.nixosTest (import ./tests/storage/btrfs-degrade.nix);
          btrfs-enospc = pkgs.testers.nixosTest (import ./tests/storage/btrfs-enospc.nix);
          samba = pkgs.testers.nixosTest (import ./tests/samba.nix);
          braid-add-disk = pkgs.testers.nixosTest (
            import ./tests/cli/braid-add-disk.nix {
              braid = linuxCrane.braid;
            }
          );
          braid-remove-disk = pkgs.testers.nixosTest (
            import ./tests/cli/braid-remove-disk.nix {
              braid = linuxCrane.braid;
            }
          );
          remove-metadata-dup = pkgs.testers.nixosTest (
            import ./tests/cli/remove-metadata-dup.nix {
              braid = linuxCrane.braid;
            }
          );
          braid-unified = pkgs.testers.nixosTest (
            import ./tests/cli/braid-unified.nix {
              braid = linuxCrane.braid;
            }
          );
          braid-status-rust = pkgs.testers.nixosTest (
            import ./tests/cli/braid-status-rust.nix {
              braid = linuxCrane.braid;
            }
          );
          luks-header-backup = pkgs.testers.nixosTest (
            import ./tests/storage/luks-header-backup.nix {
              braid = linuxCrane.braid;
            }
          );
          braid-doctor = pkgs.testers.nixosTest (
            import ./tests/cli/braid-doctor.nix {
              braid = linuxCrane.braid;
            }
          );
          shell-completion = pkgs.testers.nixosTest (
            import ./tests/cli/shell-completion.nix {
              braid = linuxCrane.braid;
            }
          );
          config-key-immutability = pkgs.testers.nixosTest (
            import ./tests/cli/config-key-immutability.nix {
              braid = linuxCrane.braid;
            }
          );
          braid-checkpoint-opstate = pkgs.testers.nixosTest (
            import ./tests/cli/braid-checkpoint-opstate.nix {
              braid = linuxCrane.braid;
            }
          );
          replace-live-disk = pkgs.testers.nixosTest (
            import ./tests/cli/replace-live-disk.nix {
              braid = linuxCrane.braid;
            }
          );
          tool-versions = pkgs.testers.nixosTest (
            import ./tests/cli/tool-versions.nix {
              braid-cli-unwrapped = linuxCrane.braid-cli-unwrapped;
            }
          );
          capture-tool-fixtures = pkgs.testers.nixosTest (import ./tests/capture-tool-fixtures.nix);
          progress-monitoring = pkgs.testers.nixosTest (import ./tests/progress-monitoring.nix);
          braid-module-disabled = pkgs.testers.nixosTest (import ./tests/module/disabled.nix);
          braid-module-single-disk = pkgs.testers.nixosTest (
            import ./tests/module/single-disk.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          braid-module-raid1 = pkgs.testers.nixosTest (
            import ./tests/module/raid1.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          braid-module-degraded-raid1 = pkgs.testers.nixosTest (
            import ./tests/module/degraded-raid1.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          braid-module-bad-config = pkgs.testers.nixosTest (
            import ./tests/module/bad-config.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          braid-module-single-disk-dead = pkgs.testers.nixosTest (
            import ./tests/module/single-disk-dead.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          braid-module-invalid-disk-keys =
            import ./tests/module/invalid-disk-keys.nix {
              inherit nixpkgs;
              system = linuxSystem;
            };
          braid-lock = pkgs.testers.nixosTest (
            import ./tests/cli/braid-lock.nix {
              braid = linuxCrane.braid;
            }
          );
          braid-unlock = pkgs.testers.nixosTest (
            import ./tests/cli/braid-unlock.nix {
              braid = linuxCrane.braid;
            }
          );
          braid-wrong-passphrase-resume = pkgs.testers.nixosTest (
            import ./tests/cli/braid-wrong-passphrase-resume.nix {
              braid = linuxCrane.braid;
            }
          );
          braid-remove-disk-busy = pkgs.testers.nixosTest (
            import ./tests/cli/braid-remove-disk-busy.nix {
              braid = linuxCrane.braid;
            }
          );
          luks-label = pkgs.testers.nixosTest (
            import ./tests/cli/luks-label.nix {
              braid = linuxCrane.braid;
            }
          );
          braid-module-no-discard =
            import ./tests/module/no-discard.nix {
              inherit nixpkgs;
              system = linuxSystem;
            };
        };
    in
    {
      nixosModules.default =
        { pkgs, lib, ... }:
        let
          # Use braid's own pinned nixpkgs for tool versions, not the consumer's
          braidPkgs = import self.inputs.nixpkgs { system = pkgs.system; };
        in
        {
          imports = [ ./modules/braid ];
          config.braid = {
            package = lib.mkDefault self.packages.${pkgs.system}.braid-cli-unwrapped;
            packages = {
              cryptsetup = lib.mkDefault braidPkgs.cryptsetup;
              btrfsProgs = lib.mkDefault braidPkgs.btrfs-progs;
              utilLinux = lib.mkDefault braidPkgs.util-linux;
              jq = lib.mkDefault braidPkgs.jq;
              coreutils = lib.mkDefault braidPkgs.coreutils;
            };
          };
        };

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
              just
            ];
          };
        }
      );
    };
}
