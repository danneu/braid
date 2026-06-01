[← braid](../index.md)

# Development

braid is developed test-first with NixOS VM tests. Tests run on macOS via `nix.linux-builder.enable = true` in nix-darwin (checks are `checks.aarch64-darwin`).

## Dev shell

Enter the pinned braid dev shell before running local commands:

```bash
nix develop
```

The shell includes the Rust toolchain (`cargo`, `rustc`, `rustfmt`, `clippy`, `rust-analyzer`), `just`, and braid's parser-critical/runtime tools (`btrfs-progs`, `cryptsetup`, `util-linux`, `nut`).

## Test workflow

```bash
# run one test while iterating
just test-vm braid-add-disk

# run a few specific tests
just test-vm braid-add-disk braid-remove-disk

# verbose VM logs (only when non-verbose output doesn't explain the failure)
just test-vm braid-add-disk -v

# full suite before finishing
just test-vm

# repro tests only
just test-repro

# all tests including repro
just test-all

# rust unit tests
just test-rust

# parser compatibility canary (CLI parsers against live VM tool output)
just test-parsers
```

Run tests without `-v` by default. Only add `-v` to a specific failing test when the output doesn't explain the failure. Never run `just test-vm -v` (all tests verbose) -- too much output to be useful.

### Unstable lane

Early-warning tests against nixos-unstable. Failures signal upcoming changes, not a contract violation.

```bash
# VM tests against nixos-unstable
just test-vm braid-add-disk --unstable
just test-all-unstable

# fixture capture + golden tests against unstable
just capture-all-fixtures-unstable
just test-rust-unstable
```

## Faster tests with tmpfs

VM tests create qcow2 disk images that hammer your SSD. Mount a dedicated tmpfs so builds happen in RAM:

```nix
# NixOS config
fileSystems."/tmp-braid" = {
  device = "tmpfs";
  fsType = "tmpfs";
  options = [ "size=16G" "mode=0755" ];
};
```

`just test-vm` automatically passes `--option build-dir /tmp-braid` when the mount exists.

## Building the CLI

```bash
nix build .#braid
```

Rust source lives in `cli/`.

## Upgrading dependencies

braid targets the latest stable NixOS release (`nixos-26.05`) and uses whatever package versions that channel provides -- no custom pins or overlays. Versions are locked to a specific nixpkgs commit in `flake.lock`.

### 1. Update nixpkgs

```bash
nix flake update
```

### 2. Check versions

```bash
nix eval --raw nixpkgs#btrfs-progs.version
nix eval --raw nixpkgs#systemd.version
nix eval --raw nixpkgs#autosuspend.version
nix eval --raw nixpkgs#cryptsetup.version
nix eval --raw nixpkgs#util-linux.version
nix eval --raw nixpkgs#smartmontools.version
```

### 3. Update vendored reference source

`reference/` contains upstream source used for code-level reference (parser behavior, output formats, config schemas). Most entries track `flake.lock` through nixpkgs-pinned tools; `nix-crate` tracks `Cargo.lock`. After a flake update, refresh the nixpkgs-pinned entries to match the new versions:

```bash
just fetch-references
```

### 4. Refresh fixtures and run tests

A nixpkgs bump can change tool output formats, which breaks parsers. Run the full validation sequence:

```bash
just capture-all-fixtures
just test-rust
just test-parsers
just test-vm
```

### 5. Update vendored crate sources

After any change that touches the `nix` line in `cli/Cargo.toml`, or any `cargo update`-driven bump to the `nix` package in `Cargo.lock`, refresh the crate source:

```bash
just fetch-references nix-crate
```
