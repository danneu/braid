{
  description = "braid — NixOS NAS with LUKS + btrfs RAID1";

  nixConfig = {
    extra-substituters = [ "https://braid.cachix.org" ];
    extra-trusted-public-keys = [ "braid.cachix.org-1:I/p7fx1z5n0+O80KzMuT7aXRdkVyHr/buZKaBu7HvJs=" ];
  };

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
          # Source from repo root so crane sees the workspace Cargo.toml + Cargo.lock,
          # while still including cli/ sources, test fixtures, and snapshots.
          src = pkgs.lib.cleanSourceWith {
            src = ./.;
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
          config-name-immutability = pkgs.testers.nixosTest (
            import ./tests/cli/config-name-immutability.nix {
              braid = linuxCrane.braid;
            }
          );
          replace-live-disk = pkgs.testers.nixosTest (
            import ./tests/cli/replace-live-disk.nix {
              braid = linuxCrane.braid;
            }
          );
          replace-dead-disk = pkgs.testers.nixosTest (
            import ./tests/cli/replace-dead-disk.nix {
              braid = linuxCrane.braid;
            }
          );
          replace-larger-disk = pkgs.testers.nixosTest (
            import ./tests/cli/replace-larger-disk.nix {
              braid = linuxCrane.braid;
            }
          );
          replace-2disk-pool = pkgs.testers.nixosTest (
            import ./tests/cli/replace-2disk-pool.nix {
              braid = linuxCrane.braid;
            }
          );
          replace-luks-label = pkgs.testers.nixosTest (
            import ./tests/cli/replace-luks-label.nix {
              braid = linuxCrane.braid;
            }
          );
          replace-sequential = pkgs.testers.nixosTest (
            import ./tests/cli/replace-sequential.nix {
              braid = linuxCrane.braid;
            }
          );
          replace-new-already-luks = pkgs.testers.nixosTest (
            import ./tests/cli/replace-new-already-luks.nix {
              braid = linuxCrane.braid;
            }
          );
          replace-passphrase-mismatch = pkgs.testers.nixosTest (
            import ./tests/cli/replace-passphrase-mismatch.nix {
              braid = linuxCrane.braid;
            }
          );
          replace-new-already-in-pool = pkgs.testers.nixosTest (
            import ./tests/cli/replace-new-already-in-pool.nix {
              braid = linuxCrane.braid;
            }
          );
          replace-new-in-pool-guard = pkgs.testers.nixosTest (
            import ./tests/cli/replace-new-in-pool-guard.nix {
              braid = linuxCrane.braid;
            }
          );
          replace-preserves-devid = pkgs.testers.nixosTest (
            import ./tests/cli/replace-preserves-devid.nix {
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
          no-silent-degraded = pkgs.testers.nixosTest (
            import ./tests/module/no-silent-degraded.nix {
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
          braid-module-invalid-disk-names =
            import ./tests/module/invalid-disk-names.nix {
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
          braid-remove-disk-busy = pkgs.testers.nixosTest (
            import ./tests/cli/braid-remove-disk-busy.nix {
              braid = linuxCrane.braid;
            }
          );
          braid-remove-enospc = pkgs.testers.nixosTest (
            import ./tests/cli/braid-remove-enospc.nix {
              braid = linuxCrane.braid;
            }
          );
          braid-remove-missing-enospc = pkgs.testers.nixosTest (
            import ./tests/cli/braid-remove-missing-enospc.nix {
              braid = linuxCrane.braid;
            }
          );
          braid-remove-missing-enospc-crash = pkgs.testers.nixosTest (
            import ./tests/cli/braid-remove-missing-enospc-crash.nix {
              braid = linuxCrane.braid;
            }
          );
          repro-btrfs-remove-enospc = pkgs.testers.nixosTest (
            import ./tests/repro/btrfs-remove-enospc.nix {
              braid = linuxCrane.braid;
            }
          );
          repro-btrfs-remove-enospc-crash = pkgs.testers.nixosTest (
            import ./tests/repro/btrfs-remove-enospc-crash.nix {
              braid = linuxCrane.braid;
            }
          );
          repro-degrade2x-read-only = pkgs.testers.nixosTest (
            import ./tests/repro/degrade2x-read-only.nix
          );
          repro-degraded-writes-single = pkgs.testers.nixosTest (
            import ./tests/repro/degraded-writes-single.nix
          );
          repro-degraded-writes-3disk = pkgs.testers.nixosTest (
            import ./tests/repro/degraded-writes-3disk.nix
          );
          repro-btrfs-replace-preserves-devid = pkgs.testers.nixosTest (
            import ./tests/repro/btrfs-replace-preserves-devid.nix
          );
          repro-btrfs-replace-rejects-smaller = pkgs.testers.nixosTest (
            import ./tests/repro/btrfs-replace-rejects-smaller-target.nix
          );
          luks-label = pkgs.testers.nixosTest (
            import ./tests/cli/luks-label.nix {
              braid = linuxCrane.braid;
            }
          );
          braid-enroll = pkgs.testers.nixosTest (
            import ./tests/cli/braid-enroll.nix {
              braid = linuxCrane.braid;
            }
          );
          braid-enroll-generate = pkgs.testers.nixosTest (
            import ./tests/cli/braid-enroll-generate.nix {
              braid = linuxCrane.braid;
            }
          );
          braid-unlock-key-file = pkgs.testers.nixosTest (
            import ./tests/cli/braid-unlock-key-file.nix {
              braid = linuxCrane.braid;
            }
          );
          braid-add-enroll = pkgs.testers.nixosTest (
            import ./tests/cli/braid-add-enroll.nix {
              braid = linuxCrane.braid;
            }
          );
          auto-unlock-key-present = pkgs.testers.nixosTest (
            import ./tests/module/auto-unlock-key-present.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          auto-unlock-key-missing = pkgs.testers.nixosTest (
            import ./tests/module/auto-unlock-key-missing.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          auto-unlock-key-wrong = pkgs.testers.nixosTest (
            import ./tests/module/auto-unlock-key-wrong.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
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

    };
}
