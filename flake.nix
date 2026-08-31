{
  description = "Manage an encrypted btrfs-RAID1 NAS on NixOS";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
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
          # while still including cli/ sources, test fixtures, snapshots, and
          # compile-time doc fixtures read by Rust tests.
          src = pkgs.lib.cleanSourceWith {
            src = ./.;
            filter =
              path: type:
              (craneLib.filterCargoSources path type)
              || (builtins.match ".*tests/fixtures/.*" path != null)
              || (builtins.match ".*\\.snap$" path != null)
              || (builtins.match ".*/docs/commands/status\\.md" path != null);
          };
          # Shared by both the unwrapped CLI and the wrapped `braid`. The wrapped
          # package extends this with `platforms = linux` since it shells out to
          # btrfs/luks tooling; the unwrapped CLI is pure Rust and builds on darwin.
          commonMeta = {
            description = "Manage an encrypted btrfs-RAID1 NAS on NixOS";
            homepage = "https://github.com/danneu/braid";
            license = pkgs.lib.licenses.mit;
            mainProgram = "braid"; # pname is braid-cli, but the binary is `braid`
          };
          commonArgs = {
            inherit src;
            # Single source of truth for the version: crane reads pname + version
            # from the crate manifest, so `cargo release` bumping cli/Cargo.toml is
            # the only version edit in the repo. MUST be ./cli/Cargo.toml -- the
            # repo-root Cargo.toml is [workspace]-only and carries no version.
            # See docs/design/decisions/029-release-process.md.
            inherit (craneLib.crateNameFromCargoToml { cargoToml = ./cli/Cargo.toml; }) pname version;
            meta = commonMeta;
          };
          cargoArtifacts = craneLib.buildDepsOnly commonArgs;
          braid-cli-unwrapped = craneLib.buildPackage (commonArgs // { inherit cargoArtifacts; });
          toolPath = pkgs.lib.makeBinPath [
            pkgs.cryptsetup
            pkgs.btrfs-progs
            pkgs.smartmontools
            pkgs.nut
            pkgs.ethtool
          ];
          braid =
            pkgs.runCommand "braid"
              {
                nativeBuildInputs = [ pkgs.makeWrapper ];
                # Wrapped binary shells out to btrfs/luks/etc, so it is Linux-only.
                meta = commonMeta // {
                  platforms = pkgs.lib.platforms.linux;
                };
              }
              ''
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
          nix-fast-build = pkgs.nix-fast-build;
        }
        // (
          if isLinux then
            let
              inherit (craneFor system) braid;
            in
            {
              inherit braid;
              default = braid;
            }
          else
            { }
        )
        // (
          if system == "aarch64-darwin" then
            {
              playground = (pkgs.testers.nixosTest (import ./vm/playground.nix)).driver;
            }
          else
            { }
        );

      devShellFor =
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          craneLib = crane.mkLib pkgs;
        in
        craneLib.devShell {
          packages = [
            pkgs.btrfs-progs
            pkgs.cryptsetup
            pkgs.just
            pkgs.nut
            pkgs.rustfmt
            pkgs.clippy
            pkgs.rust-analyzer
            pkgs.util-linux
          ];
        };

      docsShellFor =
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        pkgs.mkShell {
          packages = [
            pkgs.mdbook
            pkgs.mdbook-linkcheck2
            pkgs.mdbook-yml-header
            pkgs.just
            (pkgs.python3.withPackages (ps: [ ps.pyyaml ]))
          ];
        };

      # Cross-platform release-tool boundary used by the Mac-side `just release`
      # bump and by CI's release-notes step (pinned git-cliff). cargo-release
      # needs cargo on PATH, and the default devShell is Linux-only. The bump
      # compiles nothing; the x86_64-linux build + cachix push happen in CI on
      # the tag. The CI Rust test gate still uses the default Linux devShell
      # (craneLib) -- see docs/design/decisions/029-release-process.md.
      releaseShellFor =
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        pkgs.mkShell {
          packages = [
            pkgs.cargo-release
            pkgs.cargo
            pkgs.rustc
            pkgs.git-cliff
            pkgs.gh
            pkgs.git
            pkgs.just
          ];
        };

      checksFor =
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          # VM tests run Linux — build the binary for the right platform
          linuxSystem = builtins.replaceStrings [ "-darwin" ] [ "-linux" ] system;
          linuxPkgs = nixpkgs.legacyPackages.${linuxSystem};
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
          add-returned-disk-after-remove-missing = pkgs.testers.nixosTest (
            import ./tests/cli/add-returned-disk-after-remove-missing.nix {
              braid = linuxCrane.braid;
            }
          );
          add-degraded-then-remove-missing = pkgs.testers.nixosTest (
            import ./tests/cli/add-degraded-then-remove-missing.nix {
              braid = linuxCrane.braid;
            }
          );
          braid-add-uuid-swap-rejected = pkgs.testers.nixosTest (
            import ./tests/cli/braid-add-uuid-swap-rejected.nix {
              braid = linuxCrane.braid;
            }
          );
          braid-add-cloned-luks-header-rejected = pkgs.testers.nixosTest (
            import ./tests/cli/braid-add-cloned-luks-header-rejected.nix {
              braid = linuxCrane.braid;
            }
          );
          braid-add-cloned-luks-header-race-rejected = pkgs.testers.nixosTest (
            import ./tests/cli/braid-add-cloned-luks-header-race-rejected.nix {
              braid = linuxCrane.braid;
            }
          );
          braid-add-persists-before-balance = pkgs.testers.nixosTest (
            import ./tests/cli/braid-add-persists-before-balance.nix {
              braid = linuxCrane.braid;
            }
          );
          braid-add-warnings = pkgs.testers.nixosTest (
            import ./tests/cli/braid-add-warnings.nix {
              braid = linuxCrane.braid;
            }
          );
          braid-tui-browse = pkgs.testers.nixosTest (
            import ./tests/cli/braid-tui-browse.nix {
              braid = linuxCrane.braid;
            }
          );
          braid-tui-browse-ups-discovery = pkgs.testers.nixosTest (
            import ./tests/cli/braid-tui-browse-ups-discovery.nix {
              braid = linuxCrane.braid;
            }
          );
          braid-discover = pkgs.testers.nixosTest (
            import ./tests/cli/braid-discover.nix {
              braid = linuxCrane.braid;
            }
          );
          braid-discover-name-order = pkgs.testers.nixosTest (
            import ./tests/cli/braid-discover-name-order.nix {
              braid = linuxCrane.braid;
            }
          );
          braid-discover-empty-scan = pkgs.testers.nixosTest (
            import ./tests/cli/braid-discover-empty-scan.nix {
              braid = linuxCrane.braid;
            }
          );
          braid-discover-duplicate-identity = pkgs.testers.nixosTest (
            import ./tests/cli/braid-discover-duplicate-identity.nix {
              braid = linuxCrane.braid;
            }
          );
          multi-add = pkgs.testers.nixosTest (
            import ./tests/cli/multi-add.nix {
              braid = linuxCrane.braid;
            }
          );
          add-passphrase-mismatch = pkgs.testers.nixosTest (
            import ./tests/cli/add-passphrase-mismatch.nix {
              braid = linuxCrane.braid;
            }
          );
          confirm-then-passphrase-on-stdin = pkgs.testers.nixosTest (
            import ./tests/cli/confirm-then-passphrase-on-stdin.nix {
              braid = linuxCrane.braid;
            }
          );
          add-inhibits-suspend = pkgs.testers.nixosTest (
            import ./tests/cli/add-inhibits-suspend.nix {
              braid = linuxCrane.braid;
            }
          );
          braid-remove-disk = pkgs.testers.nixosTest (
            import ./tests/cli/braid-remove-disk.nix {
              braid = linuxCrane.braid;
            }
          );
          braid-destroy = pkgs.testers.nixosTest (
            import ./tests/cli/braid-destroy.nix {
              braid = linuxCrane.braid;
            }
          );
          remove-no-membership = pkgs.testers.nixosTest (
            import ./tests/cli/remove-no-membership.nix {
              braid = linuxCrane.braid;
            }
          );
          remove-metadata-dup = pkgs.testers.nixosTest (
            import ./tests/cli/remove-metadata-dup.nix {
              braid = linuxCrane.braid;
            }
          );
          remove-inhibits-suspend = pkgs.testers.nixosTest (
            import ./tests/cli/remove-inhibits-suspend.nix {
              braid = linuxCrane.braid;
            }
          );
          remove-missing-inhibits-suspend = pkgs.testers.nixosTest (
            import ./tests/cli/remove-missing-inhibits-suspend.nix {
              braid = linuxCrane.braid;
            }
          );
          remove-missing-2disk-rejected = pkgs.testers.nixosTest (
            import ./tests/cli/remove-missing-2disk-rejected.nix {
              braid = linuxCrane.braid;
            }
          );
          braid-status = pkgs.testers.nixosTest (
            import ./tests/cli/braid-status.nix {
              braid = linuxCrane.braid;
            }
          );
          braid-status-rust = pkgs.testers.nixosTest (
            import ./tests/cli/braid-status-rust.nix {
              braid = linuxCrane.braid;
            }
          );
          braid-status-during-balance = pkgs.testers.nixosTest (
            import ./tests/cli/braid-status-during-balance.nix {
              braid = linuxCrane.braid;
            }
          );
          braid-status-ups = pkgs.testers.nixosTest (
            import ./tests/cli/braid-status-ups.nix {
              braid = linuxCrane.braid;
            }
          );
          braid-add-during-balance = pkgs.testers.nixosTest (
            import ./tests/cli/braid-add-during-balance.nix {
              braid = linuxCrane.braid;
            }
          );
          braid-exclop-paused-balance = pkgs.testers.nixosTest (
            import ./tests/cli/braid-exclop-paused-balance.nix {
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
          braid-doctor-offline-member = pkgs.testers.nixosTest (
            import ./tests/cli/braid-doctor-offline-member.nix {
              braid = linuxCrane.braid;
            }
          );
          braid-doctor-uuid-swap = pkgs.testers.nixosTest (
            import ./tests/cli/braid-doctor-uuid-swap.nix {
              braid = linuxCrane.braid;
            }
          );
          braid-doctor-beep = pkgs.testers.nixosTest (
            import ./tests/cli/braid-doctor-beep.nix {
              braid = linuxCrane.braid;
            }
          );
          braid-doctor-foreign-luks-uuid = pkgs.testers.nixosTest (
            import ./tests/cli/braid-doctor-foreign-luks-uuid.nix {
              braid = linuxCrane.braid;
            }
          );
          braid-doctor-ups = pkgs.testers.nixosTest (
            import ./tests/module/braid-doctor-ups.nix {
              braid = linuxCrane.braid;
            }
          );
          doctor-metadata-mixed = pkgs.testers.nixosTest (
            import ./tests/cli/doctor-metadata-mixed.nix {
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
          replace-live-disk-busy = pkgs.testers.nixosTest (
            import ./tests/cli/replace-live-disk-busy.nix {
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
          replace-rejects-smaller-target = pkgs.testers.nixosTest (
            import ./tests/cli/replace-rejects-smaller-target.nix {
              braid = linuxCrane.braid;
            }
          );
          replace-2disk-pool = pkgs.testers.nixosTest (
            import ./tests/cli/replace-2disk-pool.nix {
              braid = linuxCrane.braid;
            }
          );
          replace-inhibits-suspend = pkgs.testers.nixosTest (
            import ./tests/cli/replace-inhibits-suspend.nix {
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
          replace-cloned-luks-header-rejected = pkgs.testers.nixosTest (
            import ./tests/cli/replace-cloned-luks-header-rejected.nix {
              braid = linuxCrane.braid;
            }
          );
          replace-enroll-existing-luks = pkgs.testers.nixosTest (
            import ./tests/cli/replace-enroll-existing-luks.nix {
              braid = linuxCrane.braid;
            }
          );
          replace-enroll-existing-luks-slot-conflict = pkgs.testers.nixosTest (
            import ./tests/cli/replace-enroll-existing-luks-slot-conflict.nix {
              braid = linuxCrane.braid;
            }
          );
          replace-passphrase-mismatch = pkgs.testers.nixosTest (
            import ./tests/cli/replace-passphrase-mismatch.nix {
              braid = linuxCrane.braid;
            }
          );
          replace-preformatted-luks-passphrase-mismatch = pkgs.testers.nixosTest (
            import ./tests/cli/replace-preformatted-luks-passphrase-mismatch.nix {
              braid = linuxCrane.braid;
            }
          );
          replace-new-in-pool-guard = pkgs.testers.nixosTest (
            import ./tests/cli/replace-new-in-pool-guard.nix {
              braid = linuxCrane.braid;
            }
          );
          replace-live-pool-collision-race-rejected = pkgs.testers.nixosTest (
            import ./tests/cli/replace-live-pool-collision-race-rejected.nix {
              braid = linuxCrane.braid;
            }
          );
          replace-preserves-devid = pkgs.testers.nixosTest (
            import ./tests/cli/replace-preserves-devid.nix {
              braid = linuxCrane.braid;
            }
          );
          replace-preview-warnings = pkgs.testers.nixosTest (
            import ./tests/cli/replace-preview-warnings.nix {
              braid = linuxCrane.braid;
            }
          );
          tool-versions = pkgs.testers.nixosTest (
            import ./tests/cli/tool-versions.nix {
              braid-cli-unwrapped = linuxCrane.braid-cli-unwrapped;
              braidWrappedPackage = linuxCrane.braid;
            }
          );
          capture-tool-fixtures = pkgs.testers.nixosTest (import ./tests/capture-tool-fixtures.nix);
          capture-ups-fixtures = pkgs.testers.nixosTest (import ./tests/capture-ups-fixtures.nix);
          progress-monitoring = pkgs.testers.nixosTest (import ./tests/progress-monitoring.nix);
          braid-module-disabled = pkgs.testers.nixosTest (import ./tests/module/disabled.nix);
          braid-module-add-bootstrap = pkgs.testers.nixosTest (
            import ./tests/module/add-bootstrap.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          braid-module-mkfs-block-group-tree = pkgs.testers.nixosTest (
            import ./tests/module/mkfs-block-group-tree.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          braid-module-add-locked-pool = pkgs.testers.nixosTest (
            import ./tests/module/add-locked-pool.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
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
          braid-lock = pkgs.testers.nixosTest (
            import ./tests/cli/braid-lock.nix {
              braid = linuxCrane.braid;
            }
          );
          braid-lock-name-order = pkgs.testers.nixosTest (
            import ./tests/cli/braid-lock-name-order.nix {
              braid = linuxCrane.braid;
            }
          );
          luks-mapper-drift = pkgs.testers.nixosTest (
            import ./tests/cli/luks-mapper-drift.nix {
              braid = linuxCrane.braid;
            }
          );
          status-mapper-drift = pkgs.testers.nixosTest (
            import ./tests/cli/status-mapper-drift.nix {
              braid = linuxCrane.braid;
            }
          );
          luks-lock-skipped-no-false-closed = pkgs.testers.nixosTest (
            import ./tests/cli/luks-lock-skipped-no-false-closed.nix {
              braid = linuxCrane.braid;
            }
          );
          braid-lock-orphan = pkgs.testers.nixosTest (
            import ./tests/cli/braid-lock-orphan.nix {
              braid = linuxCrane.braid;
            }
          );
          braid-lock-umount-busy = pkgs.testers.nixosTest (
            import ./tests/cli/braid-lock-umount-busy.nix {
              braid = linuxCrane.braid;
            }
          );
          braid-lock-btrfs-held = pkgs.testers.nixosTest (
            import ./tests/cli/braid-lock-btrfs-held.nix {
              braid = linuxCrane.braid;
            }
          );
          braid-lock-probe-failed = pkgs.testers.nixosTest (
            import ./tests/cli/braid-lock-probe-failed.nix {
              braid = linuxCrane.braid;
            }
          );
          braid-unlock = pkgs.testers.nixosTest (
            import ./tests/cli/braid-unlock.nix {
              braid = linuxCrane.braid;
            }
          );
          unlock-uuid-mismatch = pkgs.testers.nixosTest (
            import ./tests/cli/unlock-uuid-mismatch.nix {
              braid = linuxCrane.braid;
            }
          );
          enroll-uuid-mismatch = pkgs.testers.nixosTest (
            import ./tests/cli/enroll-uuid-mismatch.nix {
              braid = linuxCrane.braid;
            }
          );
          enroll-uuid-mismatch-midprompt = pkgs.testers.nixosTest (
            import ./tests/cli/enroll-uuid-mismatch-midprompt.nix {
              braid = linuxCrane.braid;
            }
          );
          braid-recover = pkgs.testers.nixosTest (
            import ./tests/cli/braid-recover.nix {
              braid = linuxCrane.braid;
            }
          );
          braid-recover-remove = pkgs.testers.nixosTest (
            import ./tests/cli/braid-recover-remove.nix {
              braid = linuxCrane.braid;
            }
          );
          recover-remove-missing-completed = pkgs.testers.nixosTest (
            import ./tests/cli/recover-remove-missing-completed.nix {
              braid = linuxCrane.braid;
            }
          );
          recover-bootstrap-crash = pkgs.testers.nixosTest (
            import ./tests/cli/recover-bootstrap-crash.nix {
              braid = linuxCrane.braid;
            }
          );
          recover-replace-not-started = pkgs.testers.nixosTest (
            import ./tests/cli/recover-replace-not-started.nix {
              braid = linuxCrane.braid;
            }
          );
          recover-replace-completed = pkgs.testers.nixosTest (
            import ./tests/cli/recover-replace-completed.nix {
              braid = linuxCrane.braid;
            }
          );
          recover-add-mixed-batch = pkgs.testers.nixosTest (
            import ./tests/cli/recover-add-mixed-batch.nix {
              braid = linuxCrane.braid;
            }
          );
          recover-replace-existing-luks-enroll = pkgs.testers.nixosTest (
            import ./tests/cli/recover-replace-existing-luks-enroll.nix {
              braid = linuxCrane.braid;
            }
          );
          recover-replace-existing-luks-uuid-mismatch = pkgs.testers.nixosTest (
            import ./tests/cli/recover-replace-existing-luks-uuid-mismatch.nix {
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
          braid-remove-softwarn = pkgs.testers.nixosTest (
            import ./tests/cli/braid-remove-softwarn.nix {
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
          braid-remove-missing-preflight-fails-closed = pkgs.testers.nixosTest (
            import ./tests/cli/braid-remove-missing-preflight-fails-closed.nix {
              braid = linuxCrane.braid;
            }
          );
          remove-missing-state-dir-readonly = pkgs.testers.nixosTest (
            import ./tests/cli/remove-missing-state-dir-readonly.nix {
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
          repro-scrub-error-hint = pkgs.testers.nixosTest (
            import ./tests/repro/scrub-error-hint.nix {
              braid = linuxCrane.braid;
            }
          );
          repro-degrade2x-read-only = pkgs.testers.nixosTest (import ./tests/repro/degrade2x-read-only.nix);
          repro-degraded-writes-single = pkgs.testers.nixosTest (
            import ./tests/repro/degraded-writes-single.nix
          );
          repro-degraded-writes-3disk = pkgs.testers.nixosTest (
            import ./tests/repro/degraded-writes-3disk.nix
          );
          repro-degraded-soft-balance = pkgs.testers.nixosTest (
            import ./tests/repro/degraded-soft-balance.nix
          );
          repro-btrfs-replace-preserves-devid = pkgs.testers.nixosTest (
            import ./tests/repro/btrfs-replace-preserves-devid.nix
          );
          repro-btrfs-replace-rejects-smaller = pkgs.testers.nixosTest (
            import ./tests/repro/btrfs-replace-rejects-smaller-target.nix
          );
          repro-btrfs-replace-rejected-during-scrub = pkgs.testers.nixosTest (
            import ./tests/repro/btrfs-replace-rejected-during-scrub.nix
          );
          repro-btrfs-scrub-start-rejected-during-scrub = pkgs.testers.nixosTest (
            import ./tests/repro/btrfs-scrub-start-rejected-during-scrub.nix
          );
          repro-btrfs-scrub-record-anchors-schedule = pkgs.testers.nixosTest (
            import ./tests/repro/btrfs-scrub-record-anchors-schedule.nix
          );
          repro-btrfs-replace-interrupted-mid-flight = pkgs.testers.nixosTest (
            import ./tests/repro/btrfs-replace-interrupted-mid-flight.nix {
              braid = linuxCrane.braid;
            }
          );
          repro-remove-without-balance = pkgs.testers.nixosTest (
            import ./tests/repro/remove-without-balance.nix
          );
          repro-remove-2to1-undersized-survivor = pkgs.testers.nixosTest (
            import ./tests/repro/remove-2to1-undersized-survivor.nix {
              braid = linuxCrane.braid;
            }
          );
          repro-cryptsetup-close-mounted = pkgs.testers.nixosTest (
            import ./tests/repro/cryptsetup-close-mounted.nix
          );
          repro-cryptsetup-close-btrfs-held = pkgs.testers.nixosTest (
            import ./tests/repro/cryptsetup-close-btrfs-held.nix
          );
          repro-kernel-journal-write-error = pkgs.testers.nixosTest (
            import ./tests/repro/kernel-journal-write-error.nix
          );
          repro-kernel-journal-bad-sector = pkgs.testers.nixosTest (
            import ./tests/repro/kernel-journal-bad-sector.nix
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
          add-enroll-recoverable = pkgs.testers.nixosTest (
            import ./tests/cli/add-enroll-recoverable.nix {
              braid = linuxCrane.braid;
            }
          );
          auto-unlock-key-present = pkgs.testers.nixosTest (
            import ./tests/module/auto-unlock-key-present.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          auto-unlock-runtime-dir-mode = pkgs.testers.nixosTest (
            import ./tests/module/auto-unlock-runtime-dir-mode.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          systemd-lifecycle = pkgs.testers.nixosTest (
            import ./tests/module/systemd-lifecycle.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          immutable-mountpoint = pkgs.testers.nixosTest (
            import ./tests/module/immutable-mountpoint.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          pool-lock-contention = pkgs.testers.nixosTest (
            import ./tests/module/pool-lock-contention.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          pool-lock-replace-contention = pkgs.testers.nixosTest (
            import ./tests/module/pool-lock-replace-contention.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          pool-lock-discover-contention = pkgs.testers.nixosTest (
            import ./tests/module/pool-lock-discover-contention.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          pool-lock-readonly-bypass = pkgs.testers.nixosTest (
            import ./tests/module/pool-lock-readonly-bypass.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          pool-lock-lock-contention = pkgs.testers.nixosTest (
            import ./tests/module/pool-lock-lock-contention.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          pool-lock-enroll-contention = pkgs.testers.nixosTest (
            import ./tests/module/pool-lock-enroll-contention.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          pool-lock-precedes-state-read = pkgs.testers.nixosTest (
            import ./tests/module/pool-lock-precedes-state-read.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          mutating-config-preflight-order = pkgs.testers.nixosTest (
            import ./tests/module/mutating-config-preflight-order.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          pool-lock-dry-run-bypass = pkgs.testers.nixosTest (
            import ./tests/module/pool-lock-dry-run-bypass.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          alert-state-lock = pkgs.testers.nixosTest (
            import ./tests/module/alert-state-lock.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          braid-pool-lock-released-after-sigkill = pkgs.testers.nixosTest (
            import ./tests/module/braid-pool-lock-released-after-sigkill.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          braid-pool-lock-not-inherited = pkgs.testers.nixosTest (
            import ./tests/module/braid-pool-lock-not-inherited.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          braid-lock-systemd-stop = pkgs.testers.nixosTest (
            import ./tests/module/braid-lock-systemd-stop.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          braid-lock-then-unlock-no-race = pkgs.testers.nixosTest (
            import ./tests/module/braid-lock-then-unlock-no-race.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          execstop-cleans-stale-online = pkgs.testers.nixosTest (
            import ./tests/module/execstop-cleans-stale-online.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          lock-tolerates-missing-pool-json = pkgs.testers.nixosTest (
            import ./tests/module/lock-tolerates-missing-pool-json.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          post-mount-failure-marks-online = pkgs.testers.nixosTest (
            import ./tests/module/post-mount-failure-marks-online.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          braid-lock-coordinator-race = pkgs.testers.nixosTest (
            import ./tests/module/braid-lock-coordinator-race.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          mark-online-skips-start-while-deactivating = pkgs.testers.nixosTest (
            import ./tests/module/mark-online-skips-start-while-deactivating.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          eval-lock-systemd-stop-deadline-ok = import ./tests/eval/lock-systemd-stop-deadline-assertion.nix {
            inherit
              pkgs
              linuxPkgs
              nixpkgs
              linuxSystem
              ;
          };
          eval-lock-systemd-stop-deadline-fails =
            import ./tests/eval/lock-systemd-stop-deadline-assertion-fails.nix
              {
                inherit
                  pkgs
                  linuxPkgs
                  nixpkgs
                  linuxSystem
                  ;
              };
          eval-mountpoint-rejects-bad-chars = import ./tests/eval/mountpoint-assertion-fails.nix {
            inherit
              pkgs
              linuxPkgs
              nixpkgs
              linuxSystem
              ;
          };
          eval-mountpoint-accepts-valid = import ./tests/eval/mountpoint-assertion-ok.nix {
            inherit
              pkgs
              linuxPkgs
              nixpkgs
              linuxSystem
              ;
          };
          eval-pool-bound-services-rejects-bad-names =
            import ./tests/eval/pool-bound-services-assertion-fails.nix
              {
                inherit
                  pkgs
                  linuxPkgs
                  nixpkgs
                  linuxSystem
                  ;
              };
          eval-pool-bound-services-accepts-acknowledged =
            import ./tests/eval/pool-bound-services-assertion-ok.nix
              {
                inherit
                  pkgs
                  linuxPkgs
                  nixpkgs
                  linuxSystem
                  ;
              };
          eval-auto-scrub-removed-options = import ./tests/eval/auto-scrub-removed-options.nix {
            inherit
              pkgs
              linuxPkgs
              nixpkgs
              linuxSystem
              ;
          };
          eval-auto-scrub-interval-warnings = import ./tests/eval/auto-scrub-interval-warnings.nix {
            inherit
              pkgs
              linuxPkgs
              nixpkgs
              linuxSystem
              ;
          };
          eval-grammar-parity = import ./tests/eval/grammar-parity.nix { inherit pkgs; };
          eval-nixos-module-default-supplies-package = import ./tests/eval/nixos-module-default-package.nix {
            inherit pkgs self nixpkgs;
          };
          eval-nixos-module-util-linux-host = import ./tests/eval/nixos-module-default-util-linux-host.nix {
            inherit pkgs self nixpkgs;
          };
          eval-version-matches-cargo = import ./tests/eval/version-matches-cargo.nix {
            inherit pkgs self system;
            cargoToml = ./cli/Cargo.toml; # resolves relative to flake.nix = repo root
          };
          ups-preflight-on-battery = pkgs.testers.nixosTest (
            import ./tests/module/ups-preflight-on-battery.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          ups-lb-clean-shutdown = pkgs.testers.nixosTest (
            import ./tests/module/ups-lb-clean-shutdown.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          ups-credential-lifecycle = pkgs.testers.nixosTest (
            import ./tests/module/ups-credential-lifecycle.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          ups-lb-during-replace = pkgs.testers.nixosTest (
            import ./tests/module/ups-lb-during-replace.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          ups-lb-during-remove = pkgs.testers.nixosTest (
            import ./tests/module/ups-lb-during-remove.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          ups-lb-during-remove-missing = pkgs.testers.nixosTest (
            import ./tests/module/ups-lb-during-remove-missing.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          ups-lb-during-balanced-add = pkgs.testers.nixosTest (
            import ./tests/module/ups-lb-during-balanced-add.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          auto-unlock-key-missing = pkgs.testers.nixosTest (
            import ./tests/module/auto-unlock-key-missing.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          auto-unlock-key-file-missing = pkgs.testers.nixosTest (
            import ./tests/module/auto-unlock-key-file-missing.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          auto-unlock-key-file-symlink = pkgs.testers.nixosTest (
            import ./tests/module/auto-unlock-key-file-symlink.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          auto-unlock-key-wrong = pkgs.testers.nixosTest (
            import ./tests/module/auto-unlock-key-wrong.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          braid-monitor = pkgs.testers.nixosTest (
            import ./tests/cli/braid-monitor.nix {
              braid = linuxCrane.braid;
            }
          );
          braid-monitor-enospc = pkgs.testers.nixosTest (
            import ./tests/cli/braid-monitor-enospc.nix {
              braid = linuxCrane.braid;
            }
          );
          braid-monitor-enospc-geometry = pkgs.testers.nixosTest (
            import ./tests/cli/braid-monitor-enospc-geometry.nix {
              braid = linuxCrane.braid;
            }
          );
          monitor-hot-unplug = pkgs.testers.nixosTest (
            import ./tests/cli/monitor-hot-unplug.nix {
              braid = linuxCrane.braid;
            }
          );
          braid-smartd-alert = pkgs.testers.nixosTest (
            import ./tests/cli/braid-smartd-alert.nix {
              braid = linuxCrane.braid;
            }
          );
          braid-ack-cleanup-pending = pkgs.testers.nixosTest (
            import ./tests/cli/braid-ack-cleanup-pending.nix {
              braid = linuxCrane.braid;
            }
          );
          braid-alert = pkgs.testers.nixosTest (
            import ./tests/module/braid-alert.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          braid-alert-slow-command = pkgs.testers.nixosTest (
            import ./tests/module/braid-alert-slow-command.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          braid-alert-no-beep = pkgs.testers.nixosTest (
            import ./tests/module/braid-alert-no-beep.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          braid-alert-hardened = pkgs.testers.nixosTest (
            import ./tests/module/braid-alert-hardened.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          braid-smartd-config = pkgs.testers.nixosTest (
            import ./tests/module/smartd-config.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          monitor-lifecycle = pkgs.testers.nixosTest (
            import ./tests/module/monitor-lifecycle.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          smartd-hook = pkgs.testers.nixosTest (
            import ./tests/module/smartd-hook.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          braid-idle = pkgs.testers.nixosTest (
            import ./tests/cli/braid-idle.nix {
              braid = linuxCrane.braid;
            }
          );
          braid-auto-suspend = pkgs.testers.nixosTest (
            import ./tests/module/braid-auto-suspend.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          fan-control = pkgs.testers.nixosTest (
            import ./tests/module/fan-control.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          fan-control-hotswap = pkgs.testers.nixosTest (
            import ./tests/module/fan-control-hotswap.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          fan-control-disabled = pkgs.testers.nixosTest (
            import ./tests/module/fan-control-disabled.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          braid-auto-scrub = pkgs.testers.nixosTest (
            import ./tests/module/auto-scrub.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          scrub-lifecycle = pkgs.testers.nixosTest (
            import ./tests/module/scrub-lifecycle.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          scrub-alert = pkgs.testers.nixosTest (
            import ./tests/module/scrub-alert.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          scrub-skip-retry = pkgs.testers.nixosTest (
            import ./tests/module/scrub-skip-retry.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          lock-stops-bound-consumers = pkgs.testers.nixosTest (
            import ./tests/module/lock-stops-bound-consumers.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          pool-bound-services = pkgs.testers.nixosTest (
            import ./tests/module/pool-bound-services.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
          subvol-mount-lifecycle = pkgs.testers.nixosTest (
            import ./tests/module/subvol-mount-lifecycle.nix {
              braid = linuxCrane.braid-cli-unwrapped;
            }
          );
        }
        # These 4 tests use QEMU device_del for hot-unplug simulation.
        # aarch64 QEMU's pcie.0 bus doesn't support hotplugging, so they
        # only work on x86_64 (i440fx/q35 chipset).
        // nixpkgs.lib.optionalAttrs (system == "x86_64-linux") {
          repro-kernel-journal-missing-disk-idle = pkgs.testers.nixosTest (
            import ./tests/repro/kernel-journal-missing-disk-idle.nix
          );
          repro-kernel-journal-missing-disk-io = pkgs.testers.nixosTest (
            import ./tests/repro/kernel-journal-missing-disk-io.nix
          );
          repro-udev-missing-disk-idle = pkgs.testers.nixosTest (
            import ./tests/repro/udev-missing-disk-idle.nix
          );
          repro-udev-missing-disk-io = pkgs.testers.nixosTest (import ./tests/repro/udev-missing-disk-io.nix);
        };
    in
    {
      nixosModules.default =
        { pkgs, lib, ... }:
        let
          # Fragile parser toolchain from braid's `nixpkgs` flake input,
          # instantiated cleanly (no consumer overlays). NOTE: the install docs
          # now recommend NOT setting `braid.inputs.nixpkgs.follows`. The default
          # (no follows) keeps this input on braid's pinned nixos-26.05 -- the
          # exact nixpkgs the release binary cache is built against -- so
          # consumers get a cache hit instead of recompiling braid-cli. `follows =
          # "nixpkgs"` is the closure-dedup opt-out: it redirects this input to
          # the consumer's nixpkgs (smaller closure) but forfeits release-cache
          # path identity and moves the five pinned tool versions onto the
          # consumer's nixpkgs. util-linux and systemd always resolve from the
          # consumer's pkgs. See docs/design/decisions/010-toolchain-pinning.md
          # and docs/design/decisions/029-release-process.md.
          braidPkgs = import self.inputs.nixpkgs { system = pkgs.stdenv.hostPlatform.system; };
        in
        {
          imports = [ ./modules/braid ];
          config.braid = {
            package = lib.mkDefault self.packages.${pkgs.stdenv.hostPlatform.system}.braid-cli-unwrapped;
            packages = {
              cryptsetup = lib.mkDefault braidPkgs.cryptsetup;
              btrfsProgs = lib.mkDefault braidPkgs.btrfs-progs;
              nut = lib.mkDefault braidPkgs.nut;
              smartmontools = lib.mkDefault braidPkgs.smartmontools;
              ethtool = lib.mkDefault braidPkgs.ethtool;
            };
          };
        };

      packages = forAllSystems packagesFor;

      # Linux-only runnable CLI. `nix run`/`nix build` resolve `default` and
      # the explicit `braid` app to the wrapped binary, which carries storage
      # tooling on PATH. Darwin gets no runnable app on purpose.
      apps = forAllSystems (
        system:
        let
          isLinux = builtins.match ".*-linux" system != null;
        in
        if isLinux then
          let
            braidApp = {
              type = "app";
              program = "${self.packages.${system}.braid}/bin/braid";
            };
          in
          {
            braid = braidApp;
            default = braidApp;
          }
        else
          { }
      );

      # Linux-only: the default devShell pulls in btrfs-progs/cryptsetup/nut/util-linux,
      # none of which evaluate on darwin. Use linux-builder or a Linux host.
      devShells = forAllSystems (
        system:
        let
          isLinux = builtins.match ".*-linux" system != null;
        in
        {
          docs = docsShellFor system;
          release = releaseShellFor system;
        }
        // (if isLinux then { default = devShellFor system; } else { })
      );

      checks = forAllSystems (
        system: nixpkgs.lib.filterAttrs (n: _: !(nixpkgs.lib.hasPrefix "repro-" n)) (checksFor system)
      );

      reproChecks = forAllSystems (
        system: nixpkgs.lib.filterAttrs (n: _: nixpkgs.lib.hasPrefix "repro-" n) (checksFor system)
      );

    };
}
