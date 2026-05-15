# Decision: Toolchain pinning

Status: Active

## Context

Braid's parser-critical runtime tools (btrfs-progs, cryptsetup, util-linux, NUT, smartmontools) are parsed by the Rust CLI. Output formats change between tool versions — a flake update to nixpkgs-unstable could silently break parsers. Generic helpers (coreutils, systemd) are used for basic system operations and are outside braid's parser contract. Browse has one tolerant UI-only systemd exception: it parses `systemctl list-units --output=json` for a picker and falls back to raw output on parse failure.

## Decision

Pin `flake.nix` to a specific NixOS stable release (nixos-25.11). Pin only parser-critical tools — those whose output braid parses or whose behavior is part of braid's correctness model. Generic helpers come from the consumer's system package set.

### How it works

- **Flake input**: `nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11"` — parser-critical tool packages come from this channel.
- **Module options**: `braid.packages.*` (cryptsetup, btrfsProgs, utilLinux, nut, smartmontools) default to the flake's nixpkgs but can be overridden per-system.
- **PATH wrapping**: The wrapper injects `cfg.packages.*` into PATH. Generic helpers (coreutils, systemd) are resolved from the consumer's `pkgs`, not pinned.
- **Two wrapping sites**: flake.nix wraps with `pkgs.*` defaults (for `nix run` and tests); the module wraps `cfg.package` with `cfg.packages.*` (for deployed NixOS systems where package options may be overridden).

### Operational escape hatch

Parser-critical tools are pinned by default to the flake's nixpkgs release, but `braid.packages.*` overrides are intentional — operators may need a newer upstream version for urgent bugfixes or security patches before braid's next nixpkgs bump. The override takes precedence; if the newer version changes output format, parser tests will catch it.

### Classification guideline

**Pin** when: braid parses the tool's output, or the tool's behavior is part of braid's correctness/safety model.

**Use system `pkgs`** when: the tool is a generic helper, braid doesn't parse its output as a correctness contract, and version drift is unlikely to affect correctness. The Browse Systemd picker is a UI-only exception because it parses `systemctl list-units --output=json` tolerantly and disables drill-in on parse failure.

New runtime dependencies must be classified into one of these two groups when added.

| Tool | Pinned by default? | Overrideable? | Reason |
|------|-------------------|---------------|--------|
| btrfs-progs | Yes | Yes (`braid.packages.btrfsProgs`) | Output parsed by nom combinators and serde JSON |
| cryptsetup | Yes | Yes (`braid.packages.cryptsetup`) | Output parsed by nom combinators |
| util-linux (lsblk) | Yes | Yes (`braid.packages.utilLinux`) | `lsblk` JSON output parsed by serde |
| NUT (`upsc`) | Yes | Yes (`braid.packages.nut`) | `upsc` key: value output parsed by `parse_upsc` for preflight safety and operator visibility |
| smartmontools | Yes | Yes (`braid.packages.smartmontools`) | `smartctl --json` output parsed by `parse_smartctl` |
| coreutils | No — system `pkgs` | No option | chown/chmod/realpath/stat — output not parsed |
| systemd | No — system `pkgs` | No option | systemctl/ask-password commodity behavior; Browse's list-units JSON picker is tolerant UI-only, not parser-critical |

### Upgrading tools

1. Bump the nixpkgs input to the next stable release.
2. Run `nix flake update nixpkgs`.
3. Run `make test` — the version-assertion test (`tool-versions`) catches drift.
4. Capture new golden-file fixtures from a VM -- `just capture-all-fixtures` writes under `cli/tests/fixtures/nixos-<release>/` (with `upsc/` holding the `capture-ups-fixtures` outputs). `just capture-all-fixtures-unstable` is the unstable-lane mirror.
5. Update parser tests if output format changed.

NUT specifically: `parse_upsc` depends on the `key: value` shape emitted by `pkgs.nut`'s `upsc` client (see `reference/nut/clients/upsc.c`). A nixpkgs bump that touches `networkupstools` triggers the same fixture-refresh obligation as the other pinned tools -- run `just capture-ups-fixtures` and `just test-rust` before merging. The `braid-status-ups` check under `just test-parsers` is the live-tool mirror of the golden fixtures.

## Alternatives considered

### BRAID_*_BIN environment variables

Rejected. Adds a second resolution mechanism alongside PATH. Every callsite would need to check the env var, falling back to PATH. More complexity, same result — Nix already controls PATH.

### Absolute paths in Rust (no PATH at all)

Rejected. Would require threading Nix store paths into the Rust binary at build time (via build.rs or env vars). Fragile and non-standard — NixOS convention is PATH wrapping via `makeWrapper`.

### Stay on nixpkgs-unstable

Rejected. Unstable channel updates tool versions without notice. A routine `nix flake update` could change btrfs-progs output format and break parsers silently. Stable releases change only for security fixes.

### Pin all runtime tools (blanket pinning)

Previously active, now superseded. Blanket pinning created unnecessary closure duplication for generic helpers (jq, coreutils) that braid does not parse. The `braid.packages.coreutils` option was also inconsistently wired — `storage.nix` used `pkgs.coreutils` directly, bypassing the option. Selective pinning is simpler and honest about what braid actually depends on.

## See

- [NixOS-native](006-nix-native.md) — follow NixOS conventions (PATH wrapping via makeWrapper)
- Principle 10 in [principles.md](../principles.md)
