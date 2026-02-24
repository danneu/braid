# Decision: Toolchain pinning

Status: Active

## Context

Braid's runtime tools (btrfs-progs, cryptsetup, util-linux) are parsed by both the shell script and the Rust CLI. Output formats change between tool versions — a flake update to nixpkgs-unstable could silently break parsers.

## Decision

Pin `flake.nix` to a specific NixOS stable release (nixos-25.11). Both the shell and Rust wrappers execute with an explicit PATH containing only module-controlled packages. Parser code is written against the pinned version's output format.

### How it works

- **Flake input**: `nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11"` — all tool packages come from this channel.
- **Module options**: `braid.packages.*` (cryptsetup, btrfsProgs, utilLinux, jq, coreutils) default to the flake's nixpkgs but can be overridden per-system.
- **PATH wrapping**: `writeShellApplication` (shell) and `makeWrapper` (Rust) inject only `cfg.packages.*` into PATH. No ambient system tools leak in.
- **Two wrapping sites**: flake.nix wraps with `pkgs.*` defaults (for `nix run` and tests); the module wraps `cfg.package` with `cfg.packages.*` (for deployed NixOS systems where package options may be overridden).

### Upgrading tools

1. Bump the nixpkgs input to the next stable release.
2. Run `nix flake update nixpkgs`.
3. Run `make test` — the version-assertion test (`tool-versions`) catches drift.
4. Capture new golden-file fixtures from a VM (`cli/tests/fixtures/<release>/`).
5. Update parser tests if output format changed.

## Alternatives considered

### BRAID_*_BIN environment variables

Rejected. Adds a second resolution mechanism alongside PATH. Every callsite would need to check the env var, falling back to PATH. More complexity, same result — Nix already controls PATH.

### Absolute paths in Rust (no PATH at all)

Rejected. Would require threading Nix store paths into the Rust binary at build time (via build.rs or env vars). Fragile and non-standard — NixOS convention is PATH wrapping via `makeWrapper`.

### Stay on nixpkgs-unstable

Rejected. Unstable channel updates tool versions without notice. A routine `nix flake update` could change btrfs-progs output format and break parsers silently. Stable releases change only for security fixes.

## See

- [NixOS-native](nix-native.md) — follow NixOS conventions (PATH wrapping via makeWrapper)
- Principle 10 in [principles.md](../principles.md)
