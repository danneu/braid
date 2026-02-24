# Braid Toolchain Version Pinning

## Context

Braid's runtime tools (btrfs-progs, cryptsetup, util-linux) are currently sourced from `nixpkgs-unstable`, meaning their output formats can change at any flake update. The Rust parsers use regex/strip_prefix/serde patterns against this output, but there's no contract guaranteeing what version they're parsing. This migration pins to nixos-25.11 stable and wraps all binaries with explicit PATH, so parser code can be written against known tool output.

**Design decision**: PATH wrapping only (no `BRAID_*_BIN` env vars). Both the shell script and Rust binary get their tools from Nix's PATH mechanism. No changes to command resolution in `scripts/braid.sh` or `cli/src/cmd.rs`.

---

## Phase 1: Pin flake.nix to nixos-25.11

**File**: `flake.nix:5`

Change:
```nix
nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
# →
nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
```

Run `nix flake update nixpkgs` then `make test` to surface any version-delta breakage before proceeding.

---

## Phase 2: Add package options to the module + canonical wrapped Rust package

### 2a. Module options

**File**: `modules/braid/options.nix`

Add `pkgs` to the module arguments. Add `braid.packages.*` and `braid.rustPackage` options:

```nix
{ lib, pkgs, ... }:
{
  options.braid = {
    # ... existing enable, disks, mountPoint ...

    packages = {
      cryptsetup = lib.mkPackageOption pkgs "cryptsetup" {};
      btrfs-progs = lib.mkPackageOption pkgs "btrfs-progs" {};
      util-linux = lib.mkPackageOption pkgs "util-linux" {};
      jq = lib.mkPackageOption pkgs "jq" {};
      coreutils = lib.mkPackageOption pkgs "coreutils" {};
    };

    rustPackage = lib.mkOption {
      type = lib.types.nullOr lib.types.package;
      default = null;
      description = "The braid Rust CLI package (unwrapped). Module will wrap it with tool PATH.";
    };
  };
}
```

### 2b. Canonical wrapped Rust package in flake.nix

**Binary naming issue**: Cargo.toml (`cli/Cargo.toml:7`) builds `bin/braid`. Only the test derivation renames it to `braid-rust` via `postInstall`. The module and `nix run` need a consistent wrapped package.

**Fix**: Define the wrapped+renamed package once in `flake.nix`, reuse everywhere.

**File**: `flake.nix` — in the `craneFor` function, add a wrapped variant:

```nix
craneFor = system:
  let
    pkgs = nixpkgs.legacyPackages.${system};
    craneLib = crane.mkLib pkgs;
    # ... existing src, commonArgs, cargoArtifacts ...
    braid-cli-unwrapped = craneLib.buildPackage (commonArgs // { inherit cargoArtifacts; });
  in {
    # Unwrapped: bin/braid (raw crane output)
    braid-cli-unwrapped = braid-cli-unwrapped;

    # Wrapped: bin/braid-rust, with pinned tool PATH
    braid-rust = let
      toolPath = pkgs.lib.makeBinPath [
        pkgs.cryptsetup pkgs.btrfs-progs pkgs.util-linux pkgs.jq pkgs.coreutils
      ];
    in pkgs.runCommand "braid-rust" { nativeBuildInputs = [ pkgs.makeWrapper ]; } ''
      mkdir -p $out/bin
      makeWrapper ${braid-cli-unwrapped}/bin/braid $out/bin/braid-rust \
        --prefix PATH : ${toolPath}
    '';
  };
```

This means:
- `nix run .#braid-rust` gets the wrapped binary with correct PATH
- `packages.braid-rust` is always wrapped — no drift-prone unwrapped usage
- Tests and module both consume the same wrapped package
- The rename `braid` → `braid-rust` happens at wrap time (single place)

The existing test override (`braid-rust-test` with `mv`) can be replaced by just using `braid-rust` directly.

---

## Phase 3: Thread package options into module wrappers

### 3a. cli.nix

**File**: `modules/braid/cli.nix`

Replace hardcoded `pkgs.*` with `cfg.packages.*`. The Rust binary wrapper reuses `cfg.packages.*` too:

```nix
{ config, lib, pkgs, ... }:
let
  cfg = config.braid;
  toolPackages = with cfg.packages; [ cryptsetup btrfs-progs util-linux jq coreutils ];

  braid = pkgs.writeShellApplication {
    name = "braid";
    runtimeInputs = toolPackages;
    text = builtins.readFile ../../scripts/braid.sh;
  };

  braid-rust-wrapped = pkgs.runCommand "braid-rust-module" {
    nativeBuildInputs = [ pkgs.makeWrapper ];
  } ''
    mkdir -p $out/bin
    makeWrapper ${cfg.rustPackage}/bin/braid $out/bin/braid-rust \
      --prefix PATH : ${lib.makeBinPath toolPackages}
  '';
in
{
  config = lib.mkIf cfg.enable {
    environment.etc."braid/config.json".text = builtins.toJSON {
      disks = cfg.disks;
      mountPoint = cfg.mountPoint;
    };

    environment.systemPackages = [ braid ]
      ++ lib.optional (cfg.rustPackage != null) braid-rust-wrapped;
  };
}
```

Note: `cfg.rustPackage` receives the **unwrapped** crane output (`braid-cli-unwrapped`), and the module wraps it with `cfg.packages.*`. This lets the module be the authority for which tool versions are used. The flake.nix `braid-rust` package wraps with `pkgs.*` defaults (for standalone use), while the module wraps with `cfg.packages.*` (for deployed use).

### 3b. storage.nix

**File**: `modules/braid/storage.nix` — 2 references to `pkgs.btrfs-progs`:
- Line 37: `script = "${pkgs.btrfs-progs}/bin/btrfs device scan";`
- Line 63: `script = "${pkgs.btrfs-progs}/bin/btrfs device scan";`

Change both to `${config.braid.packages.btrfs-progs}/bin/btrfs device scan`.

### 3c. remote-unlock.nix

**File**: `modules/braid/remote-unlock.nix` — 1 reference:
- Line 38: `cryptsetup = "${pkgs.cryptsetup}/bin/cryptsetup";`

Change to `${config.braid.packages.cryptsetup}/bin/cryptsetup`.

### 3d. daemon.nix — no changes needed

daemon.nix builds a Go binary that doesn't invoke any of the tool packages. Confirmed clean.

**Complete `pkgs.*` audit**: The only module references to tool packages are cli.nix (1), storage.nix (2), remote-unlock.nix (1). All accounted for above.

---

## Phase 4: Update Rust test .nix files (minimal)

**Scope control**: Only update the Rust-related tests (15, 16) to use the new wrapped package from flake.nix. Do NOT deduplicate the 11 shell-test wrappers in this change set — that's a separate cleanup to avoid conflating contract migration with test refactoring.

**File**: `tests/15-braid-plan-rust.nix`
**File**: `tests/16-braid-apply-rust.nix`

Currently these take `{ braid-rust }:` and add it directly to systemPackages. Update to use the pre-wrapped `braid-rust` from flake.nix (which already has PATH baked in):

```nix
{ braid-rust }:
{
  nodes.machine = { pkgs, ... }: {
    # braid-rust is already wrapped with tool PATH from flake.nix
    environment.systemPackages = [ braid-rust pkgs.cryptsetup pkgs.btrfs-progs pkgs.jq ];
    # ...
  };
}
```

The `braid-rust` parameter now receives the wrapped package from `checksFor`:
```nix
braid-plan-rust = pkgs.testers.nixosTest (import ./tests/15-braid-plan-rust.nix {
  braid-rust = (craneFor linuxSystem).braid-rust;  # already wrapped+renamed
});
```

Remove the old test-specific `braid-rust-test` override (the `overrideAttrs` + `mv` in `checksFor`).

**Shell-test wrapper dedup**: Deferred to a follow-up PR. The 9 shell-only test files keep their inline wrappers for now.

---

## Phase 5: Version-assertion VM test

**New files**: `tests/17-tool-versions.nix`, `tests/tool-versions.py`

### Provenance assertions (not just version strings)

The test must verify tools resolve to Nix store paths, not just check `--version` output. This catches PATH leaks where an ambient binary with the right version string could pass.

```python
# Assert tools resolve to /nix/store/ (Nix-managed, not ambient)
with subtest("btrfs resolves to nix store"):
    path = machine.succeed("readlink -f $(command -v btrfs)").strip()
    assert path.startswith("/nix/store/"), f"btrfs not from nix store: {path}"

with subtest("cryptsetup resolves to nix store"):
    path = machine.succeed("readlink -f $(command -v cryptsetup)").strip()
    assert path.startswith("/nix/store/"), f"cryptsetup not from nix store: {path}"

with subtest("btrfs version matches pinned"):
    version = machine.succeed("btrfs --version").strip()
    # Assert exact version from nixos-25.11 (fill in after Phase 1)
    assert "btrfs-progs v6." in version, f"unexpected btrfs version: {version}"

with subtest("Rust binary tool resolution"):
    # Verify braid-rust can find its tools through the wrapper PATH
    path = machine.succeed("readlink -f $(command -v braid-rust)").strip()
    assert path.startswith("/nix/store/"), f"braid-rust not from nix store: {path}"
```

Register in `flake.nix` `checksFor`:
```nix
tool-versions = pkgs.testers.nixosTest (import ./tests/17-tool-versions.nix {
  braid-rust = (craneFor linuxSystem).braid-rust;
});
```

---

## Phase 6: Parser hardening (selective)

### Parser module layout migration (by command, not by format)

Refactor parser modules from `parse/json.rs` + `parse/text.rs` into one module per CLI command contract. This is a structure-only migration first (no behavior change), followed by hardening.

Target layout:
- `cli/src/parse/lsblk.rs`
- `cli/src/parse/findmnt.rs`
- `cli/src/parse/cryptsetup_status.rs`
- `cli/src/parse/cryptsetup_luks_uuid.rs`
- `cli/src/parse/btrfs_filesystem_show.rs`
- `cli/src/parse/btrfs_filesystem_df.rs`
- `cli/src/parse/btrfs_filesystem_usage.rs`
- `cli/src/parse/btrfs_device_stats.rs`
- `cli/src/parse/btrfs_scrub_status.rs`
- shared helpers/errors in `cli/src/parse/common.rs` and `cli/src/parse/mod.rs`

Why this migration:
- Command contracts become explicit and reviewable (one file == one tool output contract).
- Fixture organization maps naturally to parser ownership.
- Version drift audits become cheaper: changed command output maps directly to one module.
- Reduces mixed concerns and accidental coupling in format-bucket files (`text.rs`/`json.rs`).

Execution notes:
- Step 1: Move code/tests with identical behavior (pure file/module reorg).
- Step 2: Repoint imports from `parse::json/text::*` to command modules.
- Step 3: Run `cargo test` to confirm zero behavior deltas.
- Step 4: Apply hardening changes (deny/leniency policies, typed classifications) on top.

### Selective `deny_unknown_fields`

Apply `#[serde(deny_unknown_fields)]` only to JSON structs where the schema is contract-critical AND where new fields would indicate a tool version change we need to investigate. Document the policy per parser.

**File**: `cli/src/parse/json.rs`

| Struct | deny_unknown_fields? | Rationale |
|--------|---------------------|-----------|
| `RawLsblkOutput` | No | lsblk JSON is queried with explicit `--output` columns; extra fields won't appear |
| `RawFindmntOutput` | No | Same — explicit `--output` columns |
| `RawBtrfsDfOutput` | **Yes** | `btrfs filesystem df --format json` outputs full schema; new fields signal version change |

For text parsers, no structural changes — the existing patterns (regex + strip_prefix) are already appropriately strict or lenient based on the output format.

### Move string matching to parser boundary

Replace remaining domain-layer `string.contains(...)` checks with typed parser results. Domain logic (`apply.rs`, `probe.rs`, `plan.rs`) should branch only on enums/structs, never raw stderr/stdout fragments.

Scope:
- `cli/src/apply.rs`: mount error classification (`is_deferable_missing_member_mount_error`)
- `cli/src/apply.rs`: btrfs-superblock probe classification (`probe_device_has_btrfs`)

Implementation direction:
- Add parser-level functions/enums in `cli/src/parse/*` for:
  - mount outcome classification (`Mounted`, `MissingMembersDeferred`, `HardError`)
  - btrfs probe classification (`HasBtrfs`, `NoBtrfs`, `ProbeError`)
- Keep all tolerant text matching inside parser modules with fixture-backed tests.
- Keep fail-hard default for unknown text variants.

Acceptance criteria:
- No raw command-output substring checks outside `cli/src/parse/*` (except tests).
- New parser classification tests cover currently observed message variants.
- Existing apply/probe behavior remains the same on known-good fixtures.

### Golden-file fixtures from nixos-25.11

**New directory**: `cli/tests/fixtures/nixos-25.11/`

After Phase 1 lands, capture actual tool output from a nixos-25.11 VM and add as fixture files. Add tests that parse them and assert specific values. These are the "golden files" that lock the parser contract to the pinned version.

Commands to capture:
- `lsblk --json --bytes --output NAME,TYPE,SIZE,MODEL,SERIAL,UUID`
- `btrfs --format json filesystem df <mount>`
- `btrfs filesystem show <mount>`
- `btrfs filesystem usage --raw <mount>`
- `btrfs device stats <mount>`
- `btrfs scrub status <mount>`
- `cryptsetup status <mapper>`
- `cryptsetup luksUUID <device>`
- `findmnt --json --output TARGET,SOURCE,FSTYPE --mountpoint <mount>`

---

## Phase 7: Documentation

**New file**: `docs/decisions/toolchain-pinning.md` (Status: Active)
- Rationale: deterministic parser behavior, reproducible builds
- Approach: nixos-25.11 pin + PATH wrapping
- Rejected alternatives: BRAID_*_BIN env vars, absolute paths in Rust
- Update process: bump nixpkgs → capture new fixtures → update parser tests

**Update**: `docs/principles.md` — add principle:
> **10. Pinned toolchain** — Runtime tool versions are pinned to a specific NixOS stable release via the flake input. Both shell and Rust wrappers execute with an explicit PATH containing only module-controlled packages. Parsers assume the output format of the pinned version. Upgrading tools requires updating golden-file fixtures and parser tests.

**Update**: `README.md` — note that runtime toolchain is pinned via flake/module.

---

## Execution Order

```
Phase 1 (pin flake) → make test
         ↓
Phase 2 (options + canonical wrapped package in flake.nix)
         ↓
Phase 3 (thread cfg.packages into cli.nix, storage.nix, remote-unlock.nix)
         ↓
Phase 4 (update Rust test .nix files — minimal, no shell test dedup)
         ↓                                    ↓ (parallel)
Phase 5 (version VM test)              Phase 7 (docs)
         ↓
Phase 6 (selective parser hardening + golden fixtures)
         ↓
make test (full suite)
```

Follow-up PR (separate): deduplicate shell-test wrappers across 9 test files.

## Verification

1. `make test` after Phase 1 — confirm pin doesn't break existing tests
2. `make test` after Phase 3 — confirm module options thread correctly
3. `make test` after Phase 4 — confirm Rust test updates work
4. `make test-one t=tool-versions` after Phase 5 — confirm provenance + version assertions
5. `cargo test` in `cli/` after Phase 6 — confirm golden fixtures parse correctly
6. `make test` final full run
