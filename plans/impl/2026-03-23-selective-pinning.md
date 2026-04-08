# Plan: Narrow runtime pinning to parser-critical tools

## Context

braid currently pins all five runtime tools (btrfs-progs, cryptsetup, util-linux, jq, coreutils) to braid's own nixpkgs input (`nixos-25.11`). This creates unnecessary closure duplication for tools that braid doesn't actually parse or depend on for correctness.

**Key findings from exploration:**
- jq is never invoked by the Rust CLI (zero grep hits in `cli/src/`). It sits in the wrapper PATH but nothing calls it.
- coreutils commands (`chown`, `chmod`, `realpath`, `stat`) are used only in NixOS module shell scripts for basic system operations — their output is never parsed.
- `storage.nix` already uses the consumer's `pkgs.coreutils` directly (not `cfg.packages.coreutils`) for `realpath` and `stat`, making `braid.packages.coreutils` a half-wired option that only controls some callsites.
- The wrapper script (`braid-wrapper.sh`) uses absolute paths for all coreutils calls (`@chownBin@`, `@chmodBin@`) — coreutils on the wrapper PATH is redundant.

**Tradeoff:**
- Pinning parser-critical tools gives reproducibility — parsers assume specific output formats.
- But users may need newer upstream fixes (security patches, kernel-compat fixes) before braid's next nixpkgs bump.
- Therefore the three pinned tools remain exposed as `braid.packages.{btrfsProgs,cryptsetup,utilLinux}` overrides — pinned by default, not hard-locked.

The change: remove jq and coreutils from braid's pinning surface entirely — drop the `braid.packages.*` options for them, remove them from wrapper PATH, and use `pkgs` directly where needed. Keep the three parser-critical tools pinned by default but explicitly overrideable.

## Changes

### 1. `modules/braid/options.nix` — remove jq and coreutils options (lines 34-35)

Delete these two lines:
```nix
jq = lib.mkPackageOption pkgs "jq" {};
coreutils = lib.mkPackageOption pkgs "coreutils" {};
```

The `packages` block becomes:
```nix
packages = {
  cryptsetup = lib.mkPackageOption pkgs "cryptsetup" {};
  btrfsProgs = lib.mkPackageOption pkgs "btrfs-progs" {};
  utilLinux = lib.mkPackageOption pkgs "util-linux" {};
};
```

### 2. `flake.nix` — standalone package wrapper (lines 48-54)

Remove `jq` and `coreutils` from `toolPath`:

```nix
toolPath = pkgs.lib.makeBinPath [
  pkgs.cryptsetup
  pkgs.btrfs-progs
  pkgs.util-linux
];
```

The Rust CLI does not invoke jq or coreutils commands.

### 3. `flake.nix` — module defaults (lines 457-462)

Remove jq and coreutils overrides:

```nix
packages = {
  cryptsetup = lib.mkDefault braidPkgs.cryptsetup;
  btrfsProgs = lib.mkDefault braidPkgs.btrfs-progs;
  utilLinux = lib.mkDefault braidPkgs.util-linux;
};
```

### 4. `modules/braid/wrapper.nix` — use `pkgs` directly (line 10, 20-21)

Replace `cfg.packages` references for jq/coreutils with `pkgs`:

Line 10 — remove jq and coreutils from toolPackages (nothing on PATH needs them):
```nix
toolPackages = with cfg.packages; [ cryptsetup btrfsProgs utilLinux ] ++ [ pkgs.systemd ];
```

Lines 20-21 — use `pkgs.coreutils` directly for absolute-path substitutions (consistent with how `storage.nix` already does it):
```nix
--subst-var-by chownBin '${pkgs.coreutils}/bin/chown' \
--subst-var-by chmodBin '${pkgs.coreutils}/bin/chmod' \
```

### 5. `docs/principles.md` — update principle 10 (line 48-50)

Replace:

> Runtime tool versions are pinned to a specific NixOS stable release via the flake input. Both shell and Rust wrappers execute with an explicit PATH containing only module-controlled packages. Parsers assume the output format of the pinned version. Upgrading tools requires updating golden-file fixtures and parser tests.

With:

> Parser-critical tools (btrfs-progs, cryptsetup, util-linux) are pinned to a specific NixOS stable release via the flake input. Wrappers execute with an explicit PATH built from module-controlled packages (`braid.packages.*`). Parsers assume the output format of the pinned version — upgrading those tools requires updating fixtures and parser tests. These pinned defaults are a compatibility baseline, not a lock; users may override `braid.packages.*` to pick up newer system versions when needed. Generic helpers (coreutils, systemd) come from the consumer's package set and are not part of braid's parser contract.

### 6. `docs/decisions/010-toolchain-pinning.md` — update to reflect selective policy

Update the Context, Decision, and How-it-works sections to describe the selective pinning policy. Key changes:
- Module options list becomes three (`cryptsetup`, `btrfsProgs`, `utilLinux`), not five.
- Add a "Classification guideline" section:

  **Pin** when: braid parses the tool's output, or the tool's behavior is part of braid's correctness/safety model.

  **Use system `pkgs`** when: the tool is a generic helper, braid doesn't parse its output, and version drift is unlikely to affect correctness.

- Add an "Operational escape hatch" subsection under "How it works":

  Parser-critical tools are pinned by default to the flake's nixpkgs release, but `braid.packages.*` overrides are intentional — operators may need a newer upstream version for urgent bugfixes or security patches before braid's next nixpkgs bump. The override takes precedence; if the newer version changes output format, parser tests will catch it.

Current classification:

| Tool | Pinned by default? | Overrideable? | Reason |
|------|-------------------|---------------|--------|
| btrfs-progs | Yes | Yes (`braid.packages.btrfsProgs`) | Output parsed by nom combinators and serde JSON |
| cryptsetup | Yes | Yes (`braid.packages.cryptsetup`) | Output parsed by nom combinators |
| util-linux (findmnt, lsblk) | Yes | Yes (`braid.packages.utilLinux`) | JSON output parsed by serde |
| coreutils | No — system `pkgs` | No option | chown/chmod/realpath/stat — output not parsed |
| jq | Removed | No option | Not invoked by braid at runtime |

### 7. `README.md` — no changes needed

Line 382 already says "runtime tools (btrfs-progs, cryptsetup, util-linux) are pinned" — lists only the three parser-critical tools. Accurate after this change.

### 8. Tests — no changes needed

`tests/cli/tool-versions.nix` and `tool-versions.py` already only assert versions and provenance for btrfs-progs, cryptsetup, and util-linux.

`tool-versions.nix` does put `pkgs.jq` and `pkgs.coreutils` in `environment.systemPackages` (lines 25-26) — those are just making the tools available in the test VM, not testing pinning. They can stay or be removed; they're harmless.

## Files to modify

| File | Change |
|------|--------|
| `modules/braid/options.nix:34-35` | Remove `jq` and `coreutils` package options |
| `flake.nix:48-54` | Remove jq, coreutils from standalone `toolPath` |
| `flake.nix:457-462` | Remove jq, coreutils from module defaults |
| `modules/braid/wrapper.nix:10,20-21` | Remove jq/coreutils from toolPackages; use `pkgs.coreutils` for chown/chmod |
| `docs/principles.md:48-50` | Update principle 10 text |
| `docs/decisions/010-toolchain-pinning.md` | Update to describe selective pinning + classification guideline |

## Verification

1. `just test-rust` — Rust unit tests still pass (no code changes in cli/).
2. `just test tool-versions` — existing pinning test passes (already only checks the three pinned tools).
3. Override escape hatch — manually verify that consumer overrides take precedence over braid's pinned defaults. In a scratch `nix repl` or eval, import the module with `braid.packages.btrfsProgs` set to a distinct derivation (e.g., `pkgs.runCommand "fake-btrfs" {} "mkdir -p $out/bin"`) and confirm the resulting wrapper's `toolPath` includes the shim's store path, not braid's pinned btrfs-progs. This is a one-time manual check, not a permanent test — the override mechanism is standard `mkDefault` precedence.
4. `just test` — full suite passes, confirming no regressions.
