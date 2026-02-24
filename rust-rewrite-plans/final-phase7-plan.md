# Phase 7: Rust Cutover — Make Rust the Primary `braid` Binary

## Context

All 4 braid CLI commands (`init-disk`, `plan`, `apply`, `status`) are fully ported to Rust and passing both unit tests and VM integration tests (tests 15–19). The bash script `scripts/braid.sh` is now redundant for all CLI functionality. Phase 7 makes the Rust binary the primary `braid` command: rename the wrapped package, update the NixOS module, make Rust tests self-contained, convert integration tests, and update docs.

No new Rust code is written in this phase — it's purely packaging, test config, module, and doc changes.

### Rollout strategy: two-step rename

To maintain bisectability, the rename `braid-rust` → `braid` is done in two commits:

**Commit 1 — Cutover**: Add `braid` alongside `braid-rust` in `craneFor`, update module, update all tests to use `braid`, update docs. Keep `braid-rust` as an alias in `craneFor` briefly. Keep bash tests 8/10/11/14 in `checksFor` for one green run.

**Commit 2 — Cleanup**: Remove `braid-rust` alias from `craneFor`, remove bash tests 8/10/11/14 from `checksFor`.

---

## 1. flake.nix packaging

### Commit 1: Add `braid`, keep `braid-rust` alias

In `craneFor`, add the new `braid` wrapped package alongside the existing `braid-rust`:

```nix
# New primary name
braid = pkgs.runCommand "braid" { nativeBuildInputs = [ pkgs.makeWrapper ]; } ''
  mkdir -p $out/bin
  makeWrapper ${braid-cli-unwrapped}/bin/braid $out/bin/braid \
    --prefix PATH : ${toolPath}
'';

# Alias for backward compat (removed in commit 2)
braid-rust = braid;
```

In `packagesFor`: expose both `braid` and `braid-rust` (commit 1), then drop `braid-rust` (commit 2).

In `checksFor`: update test registrations to pass `braid` (see section 5).

### Commit 2: Remove `braid-rust` alias

Remove `braid-rust = braid;` from `craneFor` and `braid-rust` from `packagesFor`.

---

## 2. NixOS module update

### `modules/braid/options.nix`

Rename `rustPackage` → `package`. Keep nullable (module tests for `00-disabled` don't set it):

```nix
package = lib.mkOption {
  type = lib.types.nullOr lib.types.package;
  default = null;
  description = "The braid Rust CLI package (unwrapped crane output). When set, wraps and installs as 'braid'.";
};
```

### `modules/braid/cli.nix`

Replace bash braid creation with Rust-only wrapping:

```nix
{ config, lib, pkgs, ... }:
let
  cfg = config.braid;
  toolPackages = with cfg.packages; [ cryptsetup btrfsProgs utilLinux jq coreutils ];

  braid = pkgs.runCommand "braid-module" {
    nativeBuildInputs = [ pkgs.makeWrapper ];
  } ''
    mkdir -p $out/bin
    makeWrapper ${cfg.package}/bin/braid $out/bin/braid \
      --prefix PATH : ${lib.makeBinPath toolPackages}
  '';
in
{
  config = lib.mkIf cfg.enable {
    assertions = [{
      assertion = cfg.package != null;
      message = "braid.package must be set when braid.enable is true";
    }];

    environment.etc."braid/config.json".text = builtins.toJSON {
      disks = cfg.disks;
      mountPoint = cfg.mountPoint;
    };

    environment.systemPackages = lib.optional (cfg.package != null) braid;
  };
}
```

Key changes:
- No more `builtins.readFile ../../scripts/braid.sh` — bash script no longer referenced
- No more `braid-rust-wrapped` — just `braid`
- Assertion ensures `package` is set when module is enabled

---

## 3. Rust VM tests — make self-contained

Tests 15, 16, 18, 19 currently use bash `braid` for setup and `braid-rust` for the tested command. After cutover, the Rust binary IS `braid`, so these tests use only the Rust binary.

### Nix config changes (all 4 tests)

- Parameter: `{ braid-rust }:` → `{ braid }:`
- Remove `braid-cli = pkgs.writeShellApplication { ... }` (bash braid)
- `environment.systemPackages`: replace `[ braid-cli braid-rust ... ]` with `[ braid ... ]`

### Python script changes (all 4 tests)

Global find-replace: `braid-rust` → `braid` in all command strings.

| File | Change |
|------|--------|
| `tests/15-braid-plan-rust.nix` | `{ braid }:`, remove bash braid-cli, use `braid` in packages |
| `tests/braid-plan-rust.py` | `braid-rust` → `braid` everywhere |
| `tests/16-braid-apply-rust.nix` | Same pattern |
| `tests/braid-apply-rust.py` | `braid-rust` → `braid`, merge `rust_apply`/`bash_apply` → single `apply` |
| `tests/18-braid-status-rust.nix` | Same pattern |
| `tests/braid-status-rust.py` | `braid-rust` → `braid` |
| `tests/19-braid-init-disk-rust.nix` | Same pattern |
| `tests/braid-init-disk-rust.py` | `braid-rust` → `braid` |

---

## 4. Integration tests — switch to Rust binary

These tests use `braid` by name in their Python scripts, so **Python scripts need no changes** (except test 13's warning assertion). Only the Nix configs change: accept a `{ braid }:` parameter instead of creating bash writeShellApplication.

### Test 5 (`5-braid-add-disk.nix`)

```nix
# Before: bare attrset, bash writeShellApplication inline
{ ... }

# After: accept parameter
{ braid }:
{
  name = "braid-add-disk";
  nodes.machine = { pkgs, ... }: {
    ...
    environment.systemPackages = [ braid pkgs.cryptsetup pkgs.btrfs-progs ];
    ...
  };
  ...
}
```

### Test 7 (`7-replace-failed-disk.nix`)

Same pattern: `{ braid }:` wrapper, replace bash writeShellApplication with the passed `braid` package.

### Test 9 (`9-braid-remove-disk.nix`)

Accept `{ braid }:`, replace bash `braid` with the Rust package. **Keep standalone `braid-remove-disk.sh` writeShellApplication** — that script is not ported to Rust.

### Test 12 (`12-braid-unified.nix`)

Same as test 9: accept `{ braid }:`, replace bash `braid`, keep `braid-remove-disk.sh`.

### Test 13 (`13-braid-bootstrap.nix`)

Accept `{ braid }:`, replace bash writeShellApplication with the Rust package.

**Python fix** (`tests/braid-bootstrap.py` line 56): Bash warnings are strings (`"INIT_REQUIRED: ..."`), Rust warnings are objects (`{"code": "INIT_REQUIRED", "message": "..."}`).

```python
# Before (bash format):
assert any("INIT_REQUIRED" in w for w in p["warnings"])

# After (Rust format):
assert any(w["code"] == "INIT_REQUIRED" for w in p["warnings"])
```

### Test 17 (`17-tool-versions.nix`) — module-only `braid` provenance

**Avoid dual-braid ambiguity.** Don't inject a separate wrapped `braid` package into `environment.systemPackages`. Instead, rely solely on the module-installed `braid` (via `braid.package`) and validate that `command -v braid` resolves to the module wrapper's Nix store path.

Changes:
- Parameter: `{ braid-rust, braid-cli-unwrapped }:` → `{ braid-cli-unwrapped }:` (drop the separate wrapped binary)
- Set `braid.rustPackage` → `braid.package = braid-cli-unwrapped;` (module wraps it)
- Remove `braid-rust` from `environment.systemPackages`
- Python (`tests/tool-versions.py` line 32-34): check `braid` provenance instead of `braid-rust`

```nix
# Before:
{ braid-rust, braid-cli-unwrapped }:
{
  ...
  braid.rustPackage = braid-cli-unwrapped;
  environment.systemPackages = [ braid-rust ... ];
  ...
}

# After:
{ braid-cli-unwrapped }:
{
  ...
  braid.package = braid-cli-unwrapped;
  environment.systemPackages = [ pkgs.btrfs-progs pkgs.cryptsetup pkgs.util-linux pkgs.jq pkgs.coreutils ];
  ...
}
```

In flake.nix:
```nix
tool-versions = pkgs.testers.nixosTest (import ./tests/17-tool-versions.nix {
  braid-cli-unwrapped = linuxCrane.braid-cli-unwrapped;
});
```

---

## 5. Module tests — inject Rust binary

Module tests (01–06) import `../../modules/braid`. After the module requires `braid.package`, tests that enable the module must set it.

### Pattern for each test

```nix
# Before:
{ lib, pkgs, ... }:
{
  nodes.machine = { pkgs, ... }: {
    imports = [ ../../modules/braid ];
    braid = { enable = true; disks = [ ... ]; };
    ...
  };
}

# After:
{ braid }:
{ lib, pkgs, ... }:
{
  nodes.machine = { pkgs, ... }: {
    imports = [ ../../modules/braid ];
    braid = { enable = true; package = braid; disks = [ ... ]; };
    ...
  };
}
```

Tests affected:
- `tests/braid-module/01-single-disk.nix`
- `tests/braid-module/02-raid1.nix`
- `tests/braid-module/03-degraded-raid1.nix`
- `tests/braid-module/04-bad-config.nix`
- `tests/braid-module/05-single-disk-dead.nix`
- `tests/braid-module/06-remote-unlock.nix`

**Exception:** `tests/braid-module/00-disabled.nix` does NOT enable braid, so `package` is never evaluated. No change needed — the NixOS assertion is inside `mkIf cfg.enable`.

### flake.nix check registration

Module tests pass the **unwrapped** binary (cli.nix does its own wrapping):

```nix
braid-module-single-disk = pkgs.testers.nixosTest (import ./tests/braid-module/01-single-disk.nix {
  braid = linuxCrane.braid-cli-unwrapped;
});
```

Non-module tests pass the **wrapped** binary (installed directly):

```nix
braid-plan-rust = pkgs.testers.nixosTest (import ./tests/15-braid-plan-rust.nix {
  braid = linuxCrane.braid;
});
```

---

## 6. Docs update

Update all references to `braid-rust` and `rustPackage` in user-facing docs.

### `README.md` (lines 231–244)

```markdown
# Before:
nix build .#braid-rust
`braid-rust` is built with Crane...

# After:
nix build .#braid
`braid` (the Rust CLI) is built with Crane...
```

### `docs/decisions/toolchain-pinning.md` (line 18)

```markdown
# Before:
the module wraps `cfg.rustPackage` with `cfg.packages.*`

# After:
the module wraps `cfg.package` with `cfg.packages.*`
```

**Not updated**: `rust-rewrite-plans/` and `brainstorm/` — these are historical planning docs and should reflect the terminology used at the time they were written.

---

## 7. Bash test retirement (Commit 2 only)

In commit 1, keep bash tests 8/10/11/14 in `checksFor` for one green run to confirm no regressions.

In commit 2, remove from `checksFor`:
- `braid-status` (test 8) — replaced by `braid-status-rust` (test 18)
- `braid-plan` (test 10) — replaced by `braid-plan-rust` (test 15)
- `braid-apply` (test 11) — replaced by `braid-apply-rust` (test 16)
- `braid-init-disk` (test 14) — replaced by `braid-init-disk-rust` (test 19)

Keep the .nix/.py files in the repo for historical reference. Just remove from `checksFor`.

---

## 8. Tests that need NO changes

These tests don't use the `braid` CLI at all:
- 0-hello-world, 1-luks, 2-btrfs-raid1, 3-btrfs-heal, 3-btrfs-grow, 3-btrfs-grow1, 3-btrfs-shrink, 3-btrfs-degrade
- 4-samba, 4-remote-unlock, 4-degraded-boot
- 6-first-boot-single-disk
- capture-tool-fixtures, daemon-hello-world

---

## Files modified

| File | Change |
|------|--------|
| **Packaging** | |
| `flake.nix` | Add `braid` (commit 1: keep `braid-rust` alias; commit 2: remove alias + bash checks) |
| **Module** | |
| `modules/braid/options.nix` | `rustPackage` → `package` (nullOr package, default null) |
| `modules/braid/cli.nix` | Remove bash script, wrap Rust binary as `braid`, add assertion |
| **Rust VM tests** | |
| `tests/15-braid-plan-rust.nix` | `{ braid }:`, remove bash braid-cli |
| `tests/braid-plan-rust.py` | `braid-rust` → `braid` |
| `tests/16-braid-apply-rust.nix` | `{ braid }:`, remove bash braid-cli |
| `tests/braid-apply-rust.py` | `braid-rust` → `braid`, merge apply functions |
| `tests/18-braid-status-rust.nix` | `{ braid }:`, remove bash braid-cli |
| `tests/braid-status-rust.py` | `braid-rust` → `braid` |
| `tests/19-braid-init-disk-rust.nix` | `{ braid }:`, remove bash braid-cli |
| `tests/braid-init-disk-rust.py` | `braid-rust` → `braid` |
| **Integration tests** | |
| `tests/5-braid-add-disk.nix` | Accept `{ braid }:`, use Rust binary |
| `tests/7-replace-failed-disk.nix` | Accept `{ braid }:`, use Rust binary |
| `tests/9-braid-remove-disk.nix` | Accept `{ braid }:`, use Rust binary (keep braid-remove-disk.sh) |
| `tests/12-braid-unified.nix` | Accept `{ braid }:`, use Rust binary (keep braid-remove-disk.sh) |
| `tests/13-braid-bootstrap.nix` | Accept `{ braid }:`, use Rust binary |
| `tests/braid-bootstrap.py` | Fix warning assertion: `w["code"] == "INIT_REQUIRED"` |
| `tests/17-tool-versions.nix` | Drop `braid-rust` param, module-only provenance via `braid.package` |
| `tests/tool-versions.py` | `braid-rust` → `braid` provenance check |
| **Module tests** | |
| `tests/braid-module/01-single-disk.nix` | Accept `{ braid }:`, set `braid.package` |
| `tests/braid-module/02-raid1.nix` | Same |
| `tests/braid-module/03-degraded-raid1.nix` | Same |
| `tests/braid-module/04-bad-config.nix` | Same |
| `tests/braid-module/05-single-disk-dead.nix` | Same |
| `tests/braid-module/06-remote-unlock.nix` | Same |
| **Docs** | |
| `README.md` | `.#braid-rust` → `.#braid`, update Crane cache section |
| `docs/decisions/toolchain-pinning.md` | `cfg.rustPackage` → `cfg.package` |

**NOT modified:** `scripts/braid.sh` (kept for reference), tests 0–4/6 (no braid CLI usage), `tests/braid-module/00-disabled.nix` (enable=false), `rust-rewrite-plans/` (historical).

---

## Acceptance criteria

### Functional
1. `cargo test -p braid-cli` — all unit tests pass (no Rust code changes, but verify)
2. `make test-one t=braid-plan-rust` — self-contained Rust plan test passes
3. `make test-one t=braid-apply-rust` — self-contained Rust apply test passes
4. `make test-one t=braid-status-rust` — self-contained Rust status test passes
5. `make test-one t=braid-init-disk-rust` — self-contained Rust init-disk test passes
6. `make test-one t=braid-bootstrap` — Rust bootstrap integration test passes
7. `make test-one t=braid-add-disk` — Rust integration test passes
8. `make test-one t=braid-module-single-disk` — module test with Rust binary passes
9. `make test-one t=tool-versions` — provenance test against module-installed `braid` passes
10. `make test` — full suite passes (commit 1: including bash tests; commit 2: without)

### Grep guardrails (run manually after each commit)

**After commit 1:**
11. `rg -n "braid-rust" tests/ modules/ flake.nix README.md docs/decisions/`
    - **Allowed**: `flake.nix` — the `braid-rust = braid;` alias line only
    - **Zero matches expected** in: `tests/`, `modules/`, `README.md`, `docs/decisions/`
12. `rg -n "rustPackage" modules/ tests/`
    - **Zero matches expected**
13. `rg -n "braid\.sh" modules/ flake.nix`
    - **Zero matches expected** (bash tests still in `checksFor` reference their .nix files, not braid.sh directly; but `modules/` and `flake.nix` must not reference it)

**After commit 2:**
14. `rg -n "braid-rust" tests/ modules/ flake.nix README.md docs/decisions/`
    - **Zero matches expected** everywhere (alias removed)
15. `rg -n "braid\.sh" modules/ tests/ flake.nix` — only in retired test .nix files (8, 10, 11, 14) which are no longer in `checksFor`:
    - **Allowed**: `tests/8-braid-status.nix`, `tests/10-braid-plan.nix`, `tests/11-braid-apply.nix`, `tests/14-braid-init-disk.nix`
    - **Zero matches expected** in: `modules/`, `flake.nix`, all other test files
