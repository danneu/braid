# Phase 1: Rust CLI Scaffold

## Context

Replacing `scripts/braid.sh` (~1,640 lines) with a Rust binary. Phase 1 creates the crate scaffold with domain types, config parsing, command runner trait, and clap CLI skeleton. All code templates come from `rust-rewrite-plans/phase1.md`.

## Steps

### 1. Create Rust source files

Create `cli/` directory with all source files from `phase1.md`:

- `cli/Cargo.toml` — deps: clap, serde, serde_json, thiserror, uuid
- `cli/src/lib.rs` — module declarations
- `cli/src/main.rs` — clap skeleton with 4 subcommands (all print "not yet implemented")
- `cli/src/types.rs` — newtypes (`ByIdPath`, `LuksUuid`, `MapperName`), `ActionType`, `ActionStatus` with transition validator, `PlanOutcome`, typestate `Plan<S>`, `ApplicablePlan` newtype. Inline tests for transitions, blocked plan conversion.
- `cli/src/config.rs` — `Config` struct with serde, `config_read()`, `config_hash()`, validation. Inline tests for parse/reject.
- `cli/src/cmd.rs` — `CommandRunner` trait, `RawCommandOutput`, `CmdRequest` enum, `RealRunner` (stub), `MockRunner`. Inline tests.
- `cli/src/parse.rs` — stub `parse_output()` returning `Unsupported`

### 2. Generate Cargo.lock

```bash
cd cli && cargo generate-lockfile
```

### 3. Run `cargo test` in `cli/`

Verify all inline tests pass.

### 4. Update `flake.nix`

Add `braid-rust` package using `rustPlatform.buildRustPackage`:

```nix
packagesFor = system:
  let pkgs = nixpkgs.legacyPackages.${system};
  in {
    braid-rust = pkgs.rustPlatform.buildRustPackage {
      pname = "braid-cli";
      version = "0.1.0";
      src = ./cli;
      cargoLock.lockFile = ./cli/Cargo.lock;
    };
  } // (if system == "aarch64-darwin" then {
    playground = (pkgs.testers.nixosTest (import ./vm/playground.nix)).driver;
  } else {});
```

Replace the existing `packages.aarch64-darwin.playground` with `packages = forAllSystems packagesFor;`.

### 5. Update `Makefile`

Add `test-rust` target:
```makefile
test-rust: ## Run Rust unit tests
	cd cli && cargo test
```

### 6. Verify

- `cd cli && cargo test` — all unit tests pass
- `nix build .#braid-rust` — produces `result/bin/braid`

## Key files

- `rust-rewrite-plans/phase1.md` — complete code templates (source of truth)
- `flake.nix` — add `packagesFor` with `buildRustPackage`, restructure `packages` output
- `Makefile` — add `test-rust` target
