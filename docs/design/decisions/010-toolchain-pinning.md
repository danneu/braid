---
intent: "Record the Toolchain pinning decision and rationale. Read before changing related behavior or docs."
status: Active
---
# Decision: Toolchain pinning


## Context

Braid's parser-critical runtime tools (btrfs-progs, cryptsetup, util-linux, NUT, smartmontools, ethtool) are parsed by the Rust CLI. Output formats change between tool versions -- a flake update to nixpkgs-unstable could silently break parsers. Generic helpers (coreutils, systemd) are used for basic system operations and are outside braid's parser contract. Browse has one tolerant UI-only systemd exception: it parses `systemctl list-units --output=json` for a picker and falls back to raw output on parse failure.

## Decision

Pin `flake.nix` to a specific NixOS stable release (nixos-26.05). Pin only parser-critical tools — those whose output braid parses or whose behavior is part of braid's correctness model. Generic helpers come from the consumer's system package set.

### How it works

- **Flake input**: `nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05"` — braid's own pinned channel, and the source of parser-critical tool packages unless the consumer redirects braid's `nixpkgs` input (see the follows note below).
- **Module options**: `braid.packages.*` (cryptsetup, btrfsProgs, utilLinux, nut, smartmontools, ethtool) default to braid's `nixpkgs` flake input but can be overridden per-system.
- **PATH wrapping**: The wrapper injects `cfg.packages.*` into PATH. Generic helpers (coreutils, systemd) are resolved from the consumer's `pkgs`, not pinned.
- **Two wrapping sites**: flake.nix wraps with `pkgs.*` defaults (for `nix run` and tests); the module wraps `cfg.package` with `cfg.packages.*` (for deployed NixOS systems where package options may be overridden).

### Consumer `follows` decides the actual source

`nixosModules.default` builds the `braid.packages.*` defaults with `import self.inputs.nixpkgs` -- braid's `nixpkgs` flake input, instantiated cleanly (no consumer overlays). braid's install docs recommend `braid.inputs.nixpkgs.follows = "nixpkgs"` for closure dedup, and that `follows` redirects braid's `nixpkgs` input to the consumer's nixpkgs. So in the recommended setup the pinned tools actually resolve from the *consumer's* nixpkgs; they fall back to braid's pinned nixos-26.05 only when the consumer omits the follows.

The pin therefore guarantees stable parser output only while the consumer's nixpkgs stays on the same NixOS stable release braid targets (currently nixos-26.05). Within one stable release tool output formats change only for security fixes, so a consumer aligned on braid's release is safe; a consumer who bumps nixpkgs ahead of braid moves the storage toolchain with it and re-introduces the parser-drift risk this decision otherwise prevents. Mitigate by keeping nixpkgs aligned with braid's release, pinning `braid.packages.*`, or not following braid's `nixpkgs` input.

### Operational escape hatch

Parser-critical tools are pinned by default to the flake's nixpkgs release, but `braid.packages.*` overrides are intentional -- operators may need a newer upstream version for urgent bugfixes or security patches before braid's next nixpkgs bump. The override takes precedence. Operator-set `braid.packages.*` overrides sit outside braid's committed parser contract: the standard fixture-capture and golden-test recipes build fixed flake checks against the flake's `pkgs`, so they do not validate an arbitrary override. Treating an override as supported requires a maintainer to reproduce the fixture-refresh workflow under a temporary local input swap (e.g. `--override-input nixpkgs` on the capture/test commands, or a local flake edit) at the override's package version, then re-run `just test-rust` against the resulting fixtures. Operators who skip this step are running unverified parser inputs.

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
| ethtool | Yes | Yes (`braid.packages.ethtool`) | `Wake-on:` line parsed by the doctor `wake_on_lan` check |
| coreutils | No — system `pkgs` | No option | chown/chmod/realpath/stat — output not parsed |
| systemd | No — system `pkgs` | No option | systemctl/ask-password commodity behavior; Browse's list-units JSON picker is tolerant UI-only, not parser-critical |

### Upgrading tools

A nixpkgs bump can move parser-critical tools to new output formats, so an
upgrade must refresh fixtures and re-run every parser-validation lane -- not
just confirm tool provenance. These steps mirror the canonical sequence in
[dev/overview.md](../../dev/overview.md) ("Refresh fixtures and run tests");
keep the two in sync.

1. Bump the nixpkgs input to the next stable release and run `nix flake update nixpkgs`.
2. Refresh fixtures: `just capture-all-fixtures` writes golden files under
   `cli/tests/fixtures/nixos-<release>/` (with `upsc/` holding the
   `capture-ups-fixtures` outputs). `just capture-all-fixtures-unstable` is the
   unstable-lane mirror.
3. Run the parser-validation lanes, updating parsers/tests for any output that
   changed:
   - `just test-rust` -- golden-fixture parser tests.
   - `just test-parsers` -- live-tool parser canary.
   - `just test-vm` -- VM suite. Its `tool-versions` check verifies provenance:
     each pinned tool resolves to a `/nix/store/` path on the VM's PATH and its
     self-reported version matches `pkgs.<tool>.version` from the same
     evaluation. Provenance only -- `tool-versions` does not detect that nixpkgs
     moved a tool to a new version (both sides advance together), so the fixture
     and parser tests above are the actual drift gate. Run it alone with
     `just test-vm tool-versions` for a quick provenance-only check.

NUT specifically: `parse_upsc` depends on the `key: value` shape emitted by `pkgs.nut`'s `upsc` client (see `reference/nut/clients/upsc.c`). A nixpkgs bump that touches `networkupstools` triggers the same fixture-refresh obligation as the other pinned tools -- run `just capture-ups-fixtures` and `just test-rust` before merging. The `braid-status-ups` check under `just test-parsers` is the live-tool mirror of the golden fixtures.

ethtool specifically: `wake_on_lan` depends on the `Supports Wake-on:` and `Wake-on:` lines emitted by `pkgs.ethtool`. VM virtio NICs do not provide useful Wake-on-LAN state, so there is no live fixture-capture lane; parser coverage is hand-authored in Rust unit tests, and wrapper provenance is covered by the `tool-versions` and `braid-auto-suspend` VM tests.

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
